use crate::runtime::RuntimeAdapter;
use crate::security::SecurityPolicy;
use crate::session::{backlog, ExecRun, ExecRunItem, ExecRunStatus, SessionStore};
use anyhow::{anyhow, Result};
use parking_lot::Mutex as SyncMutex;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;

const DEFAULT_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_POLL_LIMIT: u32 = 256;
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

#[derive(Debug, Clone)]
pub struct SpawnExecRunRequest {
    pub session_id: String,
    pub command: String,
    pub pty: bool,
    pub timeout_secs: i64,
    pub max_output_bytes: i64,
    pub watch_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PollExecRunResponse {
    pub run: ExecRun,
    pub items: Vec<ExecRunItem>,
    pub next_seq: i64,
}

#[derive(Debug)]
struct RunningRunControl {
    cancellation: CancellationToken,
    _handle: JoinHandle<()>,
}

#[derive(Debug, Deserialize, Clone)]
struct ExecWatcher {
    regex: String,
    event: String,
    #[serde(default)]
    once: bool,
    #[serde(default)]
    scope: Option<String>,
}

struct CompiledWatcher {
    regex: Regex,
    event: String,
    once: bool,
    fired: bool,
    scope: Option<String>,
}

struct OutputChunk {
    stream: &'static str,
    text: String,
}

pub struct ShellExecRuntime {
    store: Arc<SessionStore>,
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    running: AsyncMutex<HashMap<String, RunningRunControl>>,
    worker_notify: Notify,
    worker_started: AtomicBool,
    poll_interval: Duration,
}

impl ShellExecRuntime {
    pub fn shared(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
    ) -> Result<Arc<Self>> {
        static RUNTIMES: OnceLock<SyncMutex<HashMap<String, Weak<ShellExecRuntime>>>> =
            OnceLock::new();
        let key = security.workspace_dir.to_string_lossy().to_string();
        let runtimes = RUNTIMES.get_or_init(|| SyncMutex::new(HashMap::new()));

        if let Some(existing) = runtimes.lock().get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let store = Arc::new(SessionStore::new(&security.workspace_dir)?);
        let runtime_instance = Arc::new(Self {
            store,
            security,
            runtime,
            running: AsyncMutex::new(HashMap::new()),
            worker_notify: Notify::new(),
            worker_started: AtomicBool::new(false),
            poll_interval: DEFAULT_WORKER_POLL_INTERVAL,
        });
        runtime_instance.start_background_worker();

        runtimes
            .lock()
            .insert(key, Arc::downgrade(&runtime_instance));
        Ok(runtime_instance)
    }

    pub async fn enqueue_run(&self, request: SpawnExecRunRequest) -> Result<ExecRun> {
        let run = self.store.enqueue_exec_run(
            request.session_id.as_str(),
            request.command.as_str(),
            request.pty,
            request.timeout_secs,
            request.max_output_bytes,
            request.watch_json.as_deref(),
        )?;
        self.worker_notify.notify_waiters();
        Ok(run)
    }

    pub async fn poll_run(
        &self,
        run_id: &str,
        since_seq: Option<i64>,
    ) -> Result<Option<PollExecRunResponse>> {
        let Some(run) = self.store.get_exec_run(run_id)? else {
            return Ok(None);
        };
        let items = self
            .store
            .load_exec_run_items_since(run_id, since_seq, DEFAULT_POLL_LIMIT)?;
        let next_seq = items.last().map_or(since_seq.unwrap_or(0), |item| item.seq);

        Ok(Some(PollExecRunResponse {
            run,
            items,
            next_seq,
        }))
    }

    pub async fn stop_run(&self, run_id: &str) -> Result<Option<ExecRun>> {
        let Some(existing) = self.store.get_exec_run(run_id)? else {
            return Ok(None);
        };

        if matches!(
            ExecRunStatus::from_str(existing.status.as_str()),
            Some(ExecRunStatus::Queued | ExecRunStatus::Running)
        ) {
            if let Some(control) = self.running.lock().await.remove(run_id) {
                control.cancellation.cancel();
            }
            self.store.mark_exec_run_canceled(run_id)?;
            self.worker_notify.notify_waiters();
        }

        self.store.get_exec_run(run_id)
    }

    fn start_background_worker(self: &Arc<Self>) {
        if self.worker_started.swap(true, Ordering::AcqRel) {
            return;
        }

        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime.worker_loop().await {
                tracing::error!(error = %err, "shell exec worker loop exited");
            }
        });
    }

    async fn worker_loop(self: Arc<Self>) -> Result<()> {
        let recovered = self.store.recover_running_exec_runs_to_queued()?;
        if recovered > 0 {
            tracing::info!(
                recovered,
                "Recovered running shell exec runs to queued state"
            );
        }

        loop {
            self.claim_and_spawn_available_runs().await?;
            tokio::select! {
                _ = self.worker_notify.notified() => {}
                _ = time::sleep(self.poll_interval) => {}
            }
        }
    }

    async fn claim_and_spawn_available_runs(self: &Arc<Self>) -> Result<()> {
        loop {
            let Some(run) = self.store.claim_next_queued_exec_run()? else {
                return Ok(());
            };
            self.spawn_run(run).await;
        }
    }

    async fn spawn_run(self: &Arc<Self>, run: ExecRun) {
        let run_id = run.run_id.clone();
        let cancellation = CancellationToken::new();
        let runtime = Arc::clone(self);
        let (arm_tx, arm_rx) = oneshot::channel::<()>();
        let cancellation_for_task = cancellation.clone();

        let handle = tokio::spawn(async move {
            let _ = arm_rx.await;
            runtime
                .execute_claimed_run(run, cancellation_for_task)
                .await;
        });

        self.running.lock().await.insert(
            run_id,
            RunningRunControl {
                cancellation,
                _handle: handle,
            },
        );
        let _ = arm_tx.send(());
    }

    async fn execute_claimed_run(&self, run: ExecRun, cancellation: CancellationToken) {
        let run_id = run.run_id.clone();
        let result = self.execute_one_run(&run, cancellation.clone()).await;

        if let Err(err) = result {
            if !self.run_is_canceled(run_id.as_str()).await {
                let _ = self.store.mark_exec_run_failed(
                    run_id.as_str(),
                    None,
                    0,
                    false,
                    err.to_string().as_str(),
                );
            }
        }

        self.running.lock().await.remove(run_id.as_str());
        self.worker_notify.notify_waiters();
    }

    async fn execute_one_run(&self, run: &ExecRun, cancellation: CancellationToken) -> Result<()> {
        let mut command = self
            .runtime
            .build_shell_command(run.command.as_str(), &self.security.workspace_dir)?;
        command.env_clear();
        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                command.env(var, val);
            }
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn()?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, mut rx) = mpsc::channel::<OutputChunk>(64);
        let mut read_tasks = Vec::new();
        if let Some(stdout) = stdout {
            read_tasks.push(tokio::spawn(read_output_stream(
                stdout,
                "stdout",
                tx.clone(),
            )));
        }
        if let Some(stderr) = stderr {
            read_tasks.push(tokio::spawn(read_output_stream(
                stderr,
                "stderr",
                tx.clone(),
            )));
        }
        drop(tx);

        let mut watchers = compile_watchers(run.watch_json.as_deref())?;
        let mut output_bytes: i64 = 0;
        let mut truncated = false;
        let mut truncation_event_written = false;
        let mut timed_out = false;
        let timeout = Duration::from_secs(run.timeout_secs.max(1) as u64);
        let timeout_sleep = time::sleep(timeout);
        tokio::pin!(timeout_sleep);
        let mut exit_code: Option<i64> = None;

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    if !self.run_is_canceled(run.run_id.as_str()).await {
                        let _ = self.store.mark_exec_run_canceled(run.run_id.as_str());
                    }
                    break;
                }
                _ = &mut timeout_sleep => {
                    timed_out = true;
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    break;
                }
                maybe_chunk = rx.recv() => {
                    match maybe_chunk {
                        Some(chunk) => {
                            self.process_output_chunk(
                                run,
                                &chunk,
                                &mut watchers,
                                &mut output_bytes,
                                &mut truncated,
                                &mut truncation_event_written,
                            )?;
                        }
                        None => {
                            if let Some(status) = child.try_wait()? {
                                exit_code = status.code().map(i64::from);
                                break;
                            }
                            time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
                _ = time::sleep(Duration::from_millis(20)) => {
                    if let Some(status) = child.try_wait()? {
                        exit_code = status.code().map(i64::from);
                        while let Ok(Some(chunk)) = time::timeout(Duration::from_millis(30), rx.recv()).await {
                            self.process_output_chunk(
                                run,
                                &chunk,
                                &mut watchers,
                                &mut output_bytes,
                                &mut truncated,
                                &mut truncation_event_written,
                            )?;
                        }
                        break;
                    }
                }
            }
        }

        for task in read_tasks {
            task.abort();
        }

        if timed_out {
            self.store
                .mark_exec_run_timed_out(run.run_id.as_str(), output_bytes, truncated)?;
            return Ok(());
        }

        if self.run_is_canceled(run.run_id.as_str()).await {
            return Ok(());
        }

        if exit_code == Some(0) {
            self.store.mark_exec_run_succeeded(
                run.run_id.as_str(),
                exit_code,
                output_bytes,
                truncated,
            )?;
            return Ok(());
        }

        self.store.mark_exec_run_failed(
            run.run_id.as_str(),
            exit_code,
            output_bytes,
            truncated,
            "exec run failed",
        )?;
        Ok(())
    }

    fn process_output_chunk(
        &self,
        run: &ExecRun,
        chunk: &OutputChunk,
        watchers: &mut [CompiledWatcher],
        output_bytes: &mut i64,
        truncated: &mut bool,
        truncation_event_written: &mut bool,
    ) -> Result<()> {
        self.apply_watchers(run, watchers, chunk.stream, chunk.text.as_str())?;

        if *output_bytes >= run.max_output_bytes {
            if !*truncated {
                *truncated = true;
            }
            if !*truncation_event_written {
                let _ = self.store.append_exec_run_item(
                    run.run_id.as_str(),
                    "event",
                    "output_truncated",
                    Some(
                        json!({
                            "run_id": run.run_id,
                            "limit": run.max_output_bytes
                        })
                        .to_string()
                        .as_str(),
                    ),
                );
                *truncation_event_written = true;
            }
            return Ok(());
        }

        let remaining = (run.max_output_bytes - *output_bytes).max(0) as usize;
        let mut text = chunk.text.clone();
        if text.len() > remaining {
            text.truncate(text.floor_char_boundary(remaining));
            *truncated = true;
        }

        if !text.is_empty() {
            self.store.append_exec_run_item(
                run.run_id.as_str(),
                chunk.stream,
                text.as_str(),
                None,
            )?;
            *output_bytes += text.len() as i64;
        }

        Ok(())
    }

    fn apply_watchers(
        &self,
        run: &ExecRun,
        watchers: &mut [CompiledWatcher],
        stream: &str,
        text: &str,
    ) -> Result<()> {
        for watcher in watchers {
            if watcher.once && watcher.fired {
                continue;
            }
            if let Some(scope) = watcher.scope.as_deref() {
                if scope != stream {
                    continue;
                }
            }
            if watcher.regex.is_match(text) {
                watcher.fired = true;
                let meta_json = json!({
                    "run_id": run.run_id,
                    "stream": stream,
                    "regex": watcher.regex.as_str()
                })
                .to_string();
                self.store.append_exec_run_item(
                    run.run_id.as_str(),
                    "event",
                    watcher.event.as_str(),
                    Some(meta_json.as_str()),
                )?;
                backlog::enqueue(
                    run.session_id.as_str(),
                    format!(
                        "Shell watcher `{}` matched for run `{}`.",
                        watcher.event, run.run_id
                    ),
                );
            }
        }
        Ok(())
    }

    async fn run_is_canceled(&self, run_id: &str) -> bool {
        self.store
            .get_exec_run(run_id)
            .ok()
            .flatten()
            .is_some_and(|run| run.status == ExecRunStatus::Canceled.as_str())
    }
}

fn compile_watchers(watch_json: Option<&str>) -> Result<Vec<CompiledWatcher>> {
    let Some(watch_json) = watch_json else {
        return Ok(Vec::new());
    };
    let watchers = serde_json::from_str::<Vec<ExecWatcher>>(watch_json)
        .map_err(|err| anyhow!("Invalid watch JSON: {err}"))?;

    watchers
        .into_iter()
        .map(|watcher| {
            Ok(CompiledWatcher {
                regex: Regex::new(watcher.regex.as_str())
                    .map_err(|err| anyhow!("Invalid watch regex '{}': {err}", watcher.regex))?,
                event: watcher.event,
                once: watcher.once,
                fired: false,
                scope: watcher.scope,
            })
        })
        .collect()
}

async fn read_output_stream<R>(
    mut reader: R,
    stream: &'static str,
    tx: mpsc::Sender<OutputChunk>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0_u8; 4096];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..read]).to_string();
        if tx.send(OutputChunk { stream, text }).await.is_err() {
            break;
        }
    }
    Ok(())
}
