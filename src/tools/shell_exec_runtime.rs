//! Shell exec runtime: async run queue, worker, and event sink.
//!
//! ## Output and snippet caps
//!
//! - **Per-chunk cap** (`CHUNK_MAX_CHARS`): each stdout/stderr chunk is capped before processing
//!   and before inclusion in the callback snippet to avoid huge single-chunk amplification.
//! - **Total cap**: per-run `max_output_bytes` (from spawn request) limits total persisted output;
//!   truncation is recorded and an `output_truncated` event may be written.
//! - **Snippet cap** (`SNIPPET_MAX_CHARS`): poll response and callback payloads include a bounded
//!   recent-output snippet (tail of concatenated stdout/stderr) so size stays small and stable.
//! - **Log cap** (`MAX_LOG_LIMIT`): action=log item count per request is server-capped.

use crate::runtime::RuntimeAdapter;
use crate::security::SecurityPolicy;
use anyhow::{anyhow, Result};
use parking_lot::Mutex as SyncMutex;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;

const DEFAULT_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_POLL_LIMIT: u32 = 256;
/// Server cap for action=log item count per request.
const MAX_LOG_LIMIT: u32 = 500;
/// Maximum character count for poll snippet and callback snippet (stable, small size).
pub const SNIPPET_MAX_CHARS: usize = 4096;
/// Per-chunk cap for stdout/stderr to avoid huge single-chunk amplification.
pub const CHUNK_MAX_CHARS: usize = 65536;
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    TimedOut,
}

impl ExecRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecRun {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub command: String,
    pub pty: bool,
    pub timeout_secs: i64,
    pub max_output_bytes: i64,
    pub watch_json: Option<String>,
    pub exit_code: Option<i64>,
    pub output_bytes: i64,
    pub truncated: bool,
    pub error_message: Option<String>,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ExecRunItem {
    pub seq: i64,
    pub run_id: String,
    pub item_type: String,
    pub payload: String,
    pub meta_json: Option<String>,
    pub created_at: String,
}

/// In-memory exec run queue and items (no persistence, no failure recovery).
struct InMemoryExecRunStore {
    runs: SyncMutex<HashMap<String, ExecRun>>,
    items: SyncMutex<HashMap<String, Vec<ExecRunItem>>>,
    queue: SyncMutex<VecDeque<String>>,
    next_seq: AtomicI64,
}

fn now_iso() -> String {
    chrono::Utc::now().format("%+").to_string()
}

impl InMemoryExecRunStore {
    fn new() -> Self {
        Self {
            runs: SyncMutex::new(HashMap::new()),
            items: SyncMutex::new(HashMap::new()),
            queue: SyncMutex::new(VecDeque::new()),
            next_seq: AtomicI64::new(1),
        }
    }

    fn enqueue(
        &self,
        session_id: &str,
        command: &str,
        pty: bool,
        timeout_secs: i64,
        max_output_bytes: i64,
        watch_json: Option<&str>,
    ) -> ExecRun {
        let now = now_iso();
        let run_id = uuid::Uuid::new_v4().to_string();
        let run = ExecRun {
            run_id: run_id.clone(),
            session_id: session_id.to_string(),
            status: ExecRunStatus::Queued.as_str().to_string(),
            command: command.to_string(),
            pty,
            timeout_secs,
            max_output_bytes,
            watch_json: watch_json.map(ToOwned::to_owned),
            exit_code: None,
            output_bytes: 0,
            truncated: false,
            error_message: None,
            queued_at: now.clone(),
            started_at: None,
            finished_at: None,
            updated_at: now.clone(),
        };
        self.runs.lock().insert(run_id.clone(), run.clone());
        self.items.lock().insert(run_id.clone(), Vec::new());
        self.queue.lock().push_back(run_id);
        run
    }

    fn claim_next(&self) -> Option<ExecRun> {
        let mut queue = self.queue.lock();
        let run_id = queue.pop_front()?;
        let mut runs = self.runs.lock();
        let run = runs.get_mut(&run_id)?;
        if run.status != ExecRunStatus::Queued.as_str() {
            return None;
        }
        let now = now_iso();
        run.status = ExecRunStatus::Running.as_str().to_string();
        run.started_at = Some(now.clone());
        run.updated_at = now;
        Some(run.clone())
    }

    fn get_run(&self, run_id: &str) -> Option<ExecRun> {
        self.runs.lock().get(run_id).cloned()
    }

    fn append_item(
        &self,
        run_id: &str,
        item_type: &str,
        payload: &str,
        meta_json: Option<&str>,
    ) -> i64 {
        let now = now_iso();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let item = ExecRunItem {
            seq,
            run_id: run_id.to_string(),
            item_type: item_type.to_string(),
            payload: payload.to_string(),
            meta_json: meta_json.map(ToOwned::to_owned),
            created_at: now,
        };
        self.items.lock().get_mut(run_id).map(|vec| vec.push(item));
        seq
    }

    fn load_items_since(
        &self,
        run_id: &str,
        since_seq: Option<i64>,
        limit: u32,
    ) -> Vec<ExecRunItem> {
        let items = self.items.lock();
        let vec = match items.get(run_id) {
            Some(v) => v,
            None => return Vec::new(),
        };
        vec.iter()
            .filter(|i| since_seq.map_or(true, |s| i.seq > s))
            .take(limit as usize)
            .cloned()
            .collect()
    }

    fn mark_succeeded(
        &self,
        run_id: &str,
        exit_code: Option<i64>,
        output_bytes: i64,
        truncated: bool,
    ) {
        let now = now_iso();
        if let Some(run) = self.runs.lock().get_mut(run_id) {
            run.status = ExecRunStatus::Succeeded.as_str().to_string();
            run.exit_code = exit_code;
            run.output_bytes = output_bytes;
            run.truncated = truncated;
            run.finished_at = Some(now.clone());
            run.updated_at = now;
        }
    }

    fn mark_failed(
        &self,
        run_id: &str,
        exit_code: Option<i64>,
        output_bytes: i64,
        truncated: bool,
        error_message: &str,
    ) {
        let now = now_iso();
        if let Some(run) = self.runs.lock().get_mut(run_id) {
            run.status = ExecRunStatus::Failed.as_str().to_string();
            run.exit_code = exit_code;
            run.output_bytes = output_bytes;
            run.truncated = truncated;
            run.error_message = Some(error_message.to_string());
            run.finished_at = Some(now.clone());
            run.updated_at = now;
        }
    }

    fn mark_timed_out(&self, run_id: &str, output_bytes: i64, truncated: bool) {
        let now = now_iso();
        if let Some(run) = self.runs.lock().get_mut(run_id) {
            run.status = ExecRunStatus::TimedOut.as_str().to_string();
            run.output_bytes = output_bytes;
            run.truncated = truncated;
            run.error_message = Some("exec run timed out".to_string());
            run.finished_at = Some(now.clone());
            run.updated_at = now;
        }
    }

    fn mark_canceled(&self, run_id: &str) {
        let now = now_iso();
        if let Some(run) = self.runs.lock().get_mut(run_id) {
            run.status = ExecRunStatus::Canceled.as_str().to_string();
            run.error_message = Some("exec run canceled".to_string());
            run.finished_at = Some(now.clone());
            run.updated_at = now;
        }
    }
}

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
    /// Bounded recent-output snippet (same builder as runtime callbacks).
    pub snippet: String,
}

/// Result of action=log: items after since_seq and cursor for next request.
#[derive(Debug, Clone)]
pub struct LogExecRunResult {
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

/// Sink for exec events (status change, watcher hit, output).
pub trait ExecEventSink: Send + Sync {
    /// Called when a watcher regex matches. Include bounded snippet so consumers can react without an extra poll.
    fn on_watcher_hit(&self, run_id: &str, session_id: &str, event: &str, snippet: &str);
    /// Called when run status changes (e.g. succeeded, failed). Optional override.
    fn on_status_change(&self, _run_id: &str, _session_id: &str, _status: &str, _snippet: &str) {}
    /// Called on each output chunk (bounded). Optional override.
    fn on_output(
        &self,
        _run_id: &str,
        _session_id: &str,
        _stream: &str,
        _chunk: &str,
        _snippet: &str,
    ) {
    }
}

/// Default sink: emits watcher hits to tracing.
struct LoggingSink;

impl ExecEventSink for LoggingSink {
    fn on_watcher_hit(&self, run_id: &str, session_id: &str, event: &str, snippet: &str) {
        tracing::info!(
            run_id,
            session_id,
            event,
            snippet = snippet,
            "shell watcher matched"
        );
    }
}

pub struct ShellExecRuntime {
    exec_store: Arc<InMemoryExecRunStore>,
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    running: AsyncMutex<HashMap<String, RunningRunControl>>,
    worker_notify: Notify,
    worker_started: AtomicBool,
    poll_interval: Duration,
    event_sink: Arc<dyn ExecEventSink>,
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

        let exec_store = Arc::new(InMemoryExecRunStore::new());
        let runtime_instance = Arc::new(Self {
            exec_store: exec_store.clone(),
            security,
            runtime,
            running: AsyncMutex::new(HashMap::new()),
            worker_notify: Notify::new(),
            worker_started: AtomicBool::new(false),
            poll_interval: DEFAULT_WORKER_POLL_INTERVAL,
            event_sink: Arc::new(LoggingSink),
        });
        runtime_instance.start_background_worker();

        runtimes
            .lock()
            .insert(key, Arc::downgrade(&runtime_instance));
        Ok(runtime_instance)
    }

    pub async fn enqueue_run(&self, request: SpawnExecRunRequest) -> Result<ExecRun> {
        let run = self.exec_store.enqueue(
            request.session_id.as_str(),
            request.command.as_str(),
            request.pty,
            request.timeout_secs,
            request.max_output_bytes,
            request.watch_json.as_deref(),
        );
        self.worker_notify.notify_waiters();
        Ok(run)
    }

    pub async fn poll_run(
        &self,
        run_id: &str,
        since_seq: Option<i64>,
    ) -> Result<Option<PollExecRunResponse>> {
        let Some(run) = self.exec_store.get_run(run_id) else {
            return Ok(None);
        };
        let items = self
            .exec_store
            .load_items_since(run_id, since_seq, DEFAULT_POLL_LIMIT);
        let next_seq = items.last().map_or(since_seq.unwrap_or(0), |item| item.seq);
        let snippet = build_snippet(&items, SNIPPET_MAX_CHARS);

        Ok(Some(PollExecRunResponse {
            run,
            items,
            next_seq,
            snippet,
        }))
    }

    /// Returns incremental exec_run_items for action=log. Optional stream filter: "stdout" or "stderr".
    /// Returns None if run_id is not found.
    pub async fn log_run(
        &self,
        run_id: &str,
        since_seq: Option<i64>,
        limit: u32,
        stream_filter: Option<&str>,
    ) -> Result<Option<LogExecRunResult>> {
        if self.exec_store.get_run(run_id).is_none() {
            return Ok(None);
        }
        let limit = limit.min(MAX_LOG_LIMIT);
        let items = self.exec_store.load_items_since(run_id, since_seq, limit);
        let next_seq = items.last().map_or(since_seq.unwrap_or(0), |item| item.seq);
        let filtered: Vec<ExecRunItem> = match stream_filter {
            Some(ty) => items.into_iter().filter(|i| i.item_type == ty).collect(),
            None => items,
        };
        Ok(Some(LogExecRunResult {
            items: filtered,
            next_seq,
        }))
    }

    pub async fn stop_run(&self, run_id: &str) -> Result<Option<ExecRun>> {
        let Some(existing) = self.exec_store.get_run(run_id) else {
            return Ok(None);
        };

        if existing.status == ExecRunStatus::Queued.as_str()
            || existing.status == ExecRunStatus::Running.as_str()
        {
            if let Some(control) = self.running.lock().await.remove(run_id) {
                control.cancellation.cancel();
            }
            self.exec_store.mark_canceled(run_id);
            self.worker_notify.notify_waiters();
        }

        Ok(self.exec_store.get_run(run_id))
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
            let Some(run) = self.exec_store.claim_next() else {
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
                self.exec_store.mark_failed(
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
        let mut recent_snippet = String::new();
        let sink = Arc::clone(&self.event_sink);
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
                        self.exec_store.mark_canceled(run.run_id.as_str());
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
                                &mut recent_snippet,
                                &sink,
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
                                &mut recent_snippet,
                                &sink,
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
            self.exec_store
                .mark_timed_out(run.run_id.as_str(), output_bytes, truncated);
            return Ok(());
        }

        if self.run_is_canceled(run.run_id.as_str()).await {
            return Ok(());
        }

        if exit_code == Some(0) {
            self.exec_store
                .mark_succeeded(run.run_id.as_str(), exit_code, output_bytes, truncated);
            return Ok(());
        }

        self.exec_store.mark_failed(
            run.run_id.as_str(),
            exit_code,
            output_bytes,
            truncated,
            "exec run failed",
        );
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
        recent_snippet: &mut String,
        sink: &Arc<dyn ExecEventSink>,
    ) -> Result<()> {
        let mut text = chunk.text.clone();
        if text.len() > CHUNK_MAX_CHARS {
            let boundary = text.floor_char_boundary(CHUNK_MAX_CHARS);
            text.truncate(boundary);
        }
        if !text.is_empty() {
            recent_snippet.push_str(&text);
            if recent_snippet.len() > SNIPPET_MAX_CHARS {
                let keep = recent_snippet
                    .char_indices()
                    .nth(recent_snippet.chars().count() - SNIPPET_MAX_CHARS)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                recent_snippet.replace_range(..keep, "");
            }
        }
        self.apply_watchers(
            run,
            watchers,
            chunk.stream,
            text.as_str(),
            recent_snippet.as_str(),
            sink,
        )?;

        if *output_bytes >= run.max_output_bytes {
            if !*truncated {
                *truncated = true;
            }
            if !*truncation_event_written {
                self.exec_store.append_item(
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
        if text.len() > remaining {
            text.truncate(text.floor_char_boundary(remaining));
            *truncated = true;
        }

        if !text.is_empty() {
            self.exec_store
                .append_item(run.run_id.as_str(), chunk.stream, text.as_str(), None);
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
        snippet: &str,
        sink: &Arc<dyn ExecEventSink>,
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
                self.exec_store.append_item(
                    run.run_id.as_str(),
                    "event",
                    watcher.event.as_str(),
                    Some(meta_json.as_str()),
                );
                sink.on_watcher_hit(
                    run.run_id.as_str(),
                    run.session_id.as_str(),
                    watcher.event.as_str(),
                    snippet,
                );
            }
        }
        Ok(())
    }

    async fn run_is_canceled(&self, run_id: &str) -> bool {
        self.exec_store
            .get_run(run_id)
            .is_some_and(|run| run.status == ExecRunStatus::Canceled.as_str())
    }
}

/// Builds a bounded snippet from exec run items (stdout/stderr payloads only). Returns the tail of
/// concatenated output up to max_chars. Used by poll response and by runtime callback payloads.
pub fn build_snippet(items: &[ExecRunItem], max_chars: usize) -> String {
    let mut combined = String::new();
    for item in items {
        if item.item_type == "stdout" || item.item_type == "stderr" {
            combined.push_str(&item.payload);
        }
    }
    let total_chars = combined.chars().count();
    if total_chars <= max_chars {
        return combined;
    }
    let skip = total_chars - max_chars;
    let start = combined
        .char_indices()
        .nth(skip)
        .map(|(i, _)| i)
        .unwrap_or(0);
    combined[start..].to_string()
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

#[cfg(test)]
mod tests {
    use super::{build_snippet, SNIPPET_MAX_CHARS};
    use crate::tools::shell_exec_runtime::ExecRunItem;

    fn item(seq: i64, item_type: &str, payload: &str) -> ExecRunItem {
        ExecRunItem {
            seq,
            run_id: "run-1".to_string(),
            item_type: item_type.to_string(),
            payload: payload.to_string(),
            meta_json: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn build_snippet_returns_bounded_tail() {
        let mut items = Vec::new();
        let chunk = "a".repeat(2000);
        for i in 0..10 {
            items.push(item(i, "stdout", &chunk));
        }
        let snippet = build_snippet(&items, SNIPPET_MAX_CHARS);
        assert!(
            snippet.chars().count() <= SNIPPET_MAX_CHARS,
            "snippet must be at most {} chars, got {}",
            SNIPPET_MAX_CHARS,
            snippet.chars().count()
        );
        assert!(!snippet.is_empty());
    }

    #[test]
    fn build_snippet_ignores_event_items() {
        let items = vec![
            item(1, "stdout", "out1"),
            item(2, "event", "watcher_fired"),
            item(3, "stderr", "err1"),
        ];
        let snippet = build_snippet(&items, 100);
        assert_eq!(snippet, "out1err1");
    }

    #[test]
    fn build_snippet_small_output_unchanged() {
        let items = vec![item(1, "stdout", "hello")];
        let snippet = build_snippet(&items, 100);
        assert_eq!(snippet, "hello");
    }
}
