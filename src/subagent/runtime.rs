use crate::config::Config;
use crate::session::{SessionStore, SubagentRun, SubagentRunStatus};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::sync::CancellationToken;

const DEFAULT_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct EnqueueSubagentRunRequest {
    pub subagent_session_id: Option<String>,
    pub spec_id: Option<String>,
    pub prompt: String,
    pub input_json: Option<String>,
    pub session_meta_json: Option<String>,
}

#[derive(Debug)]
struct RunningRunControl {
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

#[derive(Deserialize)]
struct StoredSubagentSpecConfig {
    provider: String,
    model: String,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
}

#[async_trait]
pub trait SubagentRunExecutor: Send + Sync {
    async fn execute(
        &self,
        base_config: Arc<Config>,
        store: Arc<SessionStore>,
        run: SubagentRun,
        cancellation: CancellationToken,
    ) -> Result<String>;
}

#[derive(Default)]
struct AgentLoopExecutor;

#[async_trait]
impl SubagentRunExecutor for AgentLoopExecutor {
    async fn execute(
        &self,
        base_config: Arc<Config>,
        store: Arc<SessionStore>,
        run: SubagentRun,
        cancellation: CancellationToken,
    ) -> Result<String> {
        let (run_config, run_prompt) =
            resolve_execution_inputs(base_config.as_ref(), store.as_ref(), &run)?;

        tokio::select! {
            _ = cancellation.cancelled() => Err(anyhow!("subagent run canceled")),
            result = crate::agent::loop_::process_message(run_config, &run_prompt) => {
                let response = result?;
                Ok(json!({"text": response}).to_string())
            }
        }
    }
}

fn resolve_execution_inputs(
    base_config: &Config,
    store: &SessionStore,
    run: &SubagentRun,
) -> Result<(Config, String)> {
    let mut run_config = base_config.clone();
    let mut prompt = run.prompt.clone();

    if let Some(input_json) = run.input_json.as_deref() {
        let trimmed = input_json.trim();
        if !trimmed.is_empty() {
            prompt = format!("{prompt}\n\n[Input JSON]\n{trimmed}");
        }
    }

    let session = store
        .get_subagent_session(run.subagent_session_id.as_str())?
        .ok_or_else(|| anyhow!("subagent session not found: {}", run.subagent_session_id))?;

    if let Some(spec_id) = session.spec_id {
        let spec = store
            .get_subagent_spec_by_id(spec_id.as_str())?
            .ok_or_else(|| anyhow!("subagent spec not found: {spec_id}"))?;
        let spec_config: StoredSubagentSpecConfig = serde_json::from_str(spec.config_json.as_str())
            .with_context(|| format!("invalid subagent spec JSON for '{}'", spec.name))?;

        run_config.default_provider = Some(spec_config.provider);
        run_config.default_model = Some(spec_config.model);
        if let Some(temperature) = spec_config.temperature {
            run_config.default_temperature = temperature;
        }
        if let Some(api_key) = spec_config.api_key {
            if !api_key.trim().is_empty() {
                run_config.api_key = Some(api_key);
            }
        }
        if let Some(system_prompt) = spec_config.system_prompt {
            let trimmed = system_prompt.trim();
            if !trimmed.is_empty() {
                prompt = format!("[Subagent system instructions]\n{trimmed}\n\n{prompt}");
            }
        }
    }

    Ok((run_config, prompt))
}

pub struct SubagentRuntime {
    store: Arc<SessionStore>,
    config: Arc<Config>,
    executor: Arc<dyn SubagentRunExecutor>,
    running: AsyncMutex<HashMap<String, RunningRunControl>>,
    worker_notify: Notify,
    worker_started: AtomicBool,
    poll_interval: Duration,
}

impl SubagentRuntime {
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self::with_executor(
            store,
            Arc::new(Config::default()),
            Arc::new(AgentLoopExecutor),
            DEFAULT_WORKER_POLL_INTERVAL,
        )
    }

    pub fn with_config(store: Arc<SessionStore>, config: Arc<Config>) -> Self {
        Self::with_executor(
            store,
            config,
            Arc::new(AgentLoopExecutor),
            DEFAULT_WORKER_POLL_INTERVAL,
        )
    }

    pub fn shared(config: Arc<Config>) -> Result<Arc<Self>> {
        static RUNTIMES: OnceLock<SyncMutex<HashMap<String, Weak<SubagentRuntime>>>> =
            OnceLock::new();
        let key = config.workspace_dir.to_string_lossy().to_string();
        let runtimes = RUNTIMES.get_or_init(|| SyncMutex::new(HashMap::new()));

        if let Some(existing) = runtimes.lock().get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let store = Arc::new(SessionStore::new(&config.workspace_dir)?);
        let runtime = Arc::new(Self::with_config(store, config));
        runtime.start_background_worker();

        runtimes.lock().insert(key, Arc::downgrade(&runtime));
        Ok(runtime)
    }

    #[cfg(test)]
    fn with_executor(
        store: Arc<SessionStore>,
        config: Arc<Config>,
        executor: Arc<dyn SubagentRunExecutor>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            store,
            config,
            executor,
            running: AsyncMutex::new(HashMap::new()),
            worker_notify: Notify::new(),
            worker_started: AtomicBool::new(false),
            poll_interval,
        }
    }

    #[cfg(not(test))]
    fn with_executor(
        store: Arc<SessionStore>,
        config: Arc<Config>,
        executor: Arc<dyn SubagentRunExecutor>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            store,
            config,
            executor,
            running: AsyncMutex::new(HashMap::new()),
            worker_notify: Notify::new(),
            worker_started: AtomicBool::new(false),
            poll_interval,
        }
    }

    pub fn start_background_worker(self: &Arc<Self>) {
        if self.worker_started.swap(true, Ordering::AcqRel) {
            return;
        }

        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = runtime.worker_loop().await {
                tracing::error!(error = %err, "subagent worker loop exited");
            }
        });
    }

    pub async fn enqueue_run(&self, request: EnqueueSubagentRunRequest) -> Result<SubagentRun> {
        let subagent_session_id = if let Some(existing) = request.subagent_session_id {
            existing
        } else {
            self.store
                .create_subagent_session(
                    request.spec_id.as_deref(),
                    request.session_meta_json.as_deref(),
                )?
                .subagent_session_id
        };

        let run = self.store.enqueue_subagent_run(
            &subagent_session_id,
            &request.prompt,
            request.input_json.as_deref(),
        )?;
        self.worker_notify.notify_waiters();
        Ok(run)
    }

    pub async fn stop_run(&self, run_id: &str) -> Result<Option<SubagentRun>> {
        let Some(existing) = self.store.get_subagent_run(run_id)? else {
            return Ok(None);
        };

        if matches!(
            SubagentRunStatus::from_str(existing.status.as_str()),
            Some(SubagentRunStatus::Queued | SubagentRunStatus::Running)
        ) {
            if let Some(control) = self.running.lock().await.remove(run_id) {
                control.cancellation.cancel();
                control.handle.abort();
            }
            self.store.mark_subagent_run_canceled(run_id)?;
            self.worker_notify.notify_waiters();
            return self.poll_run(run_id).await;
        }

        Ok(Some(existing))
    }

    pub async fn process_next_run(&self) -> Result<Option<SubagentRun>> {
        self.process_next_run_with(|_run| {
            Ok(json!({ "text": "subagent run completed" }).to_string())
        })
        .await
    }

    pub async fn process_next_run_with<F>(&self, execute: F) -> Result<Option<SubagentRun>>
    where
        F: Fn(&SubagentRun) -> Result<String>,
    {
        let Some(run) = self.store.claim_next_queued_subagent_run()? else {
            return Ok(None);
        };

        match execute(&run) {
            Ok(output_json) => {
                self.store
                    .mark_subagent_run_succeeded(run.run_id.as_str(), output_json.as_str())?;
            }
            Err(err) => {
                self.store
                    .mark_subagent_run_failed(run.run_id.as_str(), &err.to_string())?;
            }
        }

        self.poll_run(run.run_id.as_str()).await
    }

    pub async fn recover_incomplete_runs(&self) -> Result<usize> {
        self.store.recover_running_subagent_runs_to_queued()
    }

    pub async fn poll_run(&self, run_id: &str) -> Result<Option<SubagentRun>> {
        self.store.get_subagent_run(run_id)
    }

    pub async fn run_loop(&self, poll_interval: Duration) -> Result<()> {
        let mut interval = time::interval(poll_interval);
        loop {
            interval.tick().await;
            let _ = self.process_next_run().await?;
        }
    }

    async fn worker_loop(self: Arc<Self>) -> Result<()> {
        let recovered = self.recover_incomplete_runs().await?;
        if recovered > 0 {
            tracing::info!(recovered, "Recovered running subagent runs to queued state");
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
            let Some(run) = self.store.claim_next_queued_subagent_run()? else {
                return Ok(());
            };
            self.spawn_run(run).await;
        }
    }

    async fn spawn_run(self: &Arc<Self>, run: SubagentRun) {
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
                handle,
            },
        );
        let _ = arm_tx.send(());
    }

    async fn execute_claimed_run(&self, run: SubagentRun, cancellation: CancellationToken) {
        let run_id = run.run_id.clone();
        let result = self
            .executor
            .execute(
                Arc::clone(&self.config),
                Arc::clone(&self.store),
                run,
                cancellation.clone(),
            )
            .await;

        let canceled = cancellation.is_cancelled() || self.run_is_canceled(run_id.as_str()).await;
        if !canceled {
            match result {
                Ok(output_json) => {
                    if let Err(err) = self
                        .store
                        .mark_subagent_run_succeeded(run_id.as_str(), output_json.as_str())
                    {
                        if !self.run_is_canceled(run_id.as_str()).await {
                            tracing::warn!(run_id = %run_id, error = %err, "failed to mark subagent run succeeded");
                        }
                    }
                }
                Err(err) => {
                    if let Err(mark_err) = self
                        .store
                        .mark_subagent_run_failed(run_id.as_str(), err.to_string().as_str())
                    {
                        if !self.run_is_canceled(run_id.as_str()).await {
                            tracing::warn!(run_id = %run_id, error = %mark_err, "failed to mark subagent run failed");
                        }
                    }
                }
            }
        }

        self.running.lock().await.remove(run_id.as_str());
        self.worker_notify.notify_waiters();
    }

    async fn run_is_canceled(&self, run_id: &str) -> bool {
        self.store
            .get_subagent_run(run_id)
            .ok()
            .flatten()
            .map_or(false, |run| {
                run.status == SubagentRunStatus::Canceled.as_str()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EnqueueSubagentRunRequest, SubagentRunExecutor, SubagentRuntime,
        DEFAULT_WORKER_POLL_INTERVAL,
    };
    use crate::config::Config;
    use crate::session::{SessionStore, SubagentRun, SubagentRunStatus};
    use anyhow::Result;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::sync::{oneshot, Notify};
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;

    struct ImmediateExecutor;

    #[async_trait]
    impl SubagentRunExecutor for ImmediateExecutor {
        async fn execute(
            &self,
            _base_config: Arc<Config>,
            _store: Arc<SessionStore>,
            run: SubagentRun,
            _cancellation: CancellationToken,
        ) -> Result<String> {
            Ok(serde_json::json!({"text": format!("done:{}", run.prompt)}).to_string())
        }
    }

    struct BlockingFirstExecutor {
        first_blocked: AtomicBool,
        started_tx: Mutex<Option<oneshot::Sender<()>>>,
        release_first: Arc<Notify>,
    }

    #[async_trait]
    impl SubagentRunExecutor for BlockingFirstExecutor {
        async fn execute(
            &self,
            _base_config: Arc<Config>,
            _store: Arc<SessionStore>,
            _run: SubagentRun,
            cancellation: CancellationToken,
        ) -> Result<String> {
            if self.first_blocked.swap(false, Ordering::AcqRel) {
                if let Some(started_tx) = self.started_tx.lock().take() {
                    let _ = started_tx.send(());
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(anyhow::anyhow!("canceled")),
                    _ = self.release_first.notified() => {}
                }
            }

            Ok(serde_json::json!({"text":"ok"}).to_string())
        }
    }

    struct TrackingExecutor {
        delay: Duration,
        active_global: AtomicUsize,
        max_global: AtomicUsize,
        active_per_session: Mutex<HashMap<String, usize>>,
        max_per_session: Mutex<HashMap<String, usize>>,
    }

    impl TrackingExecutor {
        fn bump_max(max: &AtomicUsize, value: usize) {
            let mut observed = max.load(Ordering::Acquire);
            while value > observed {
                match max.compare_exchange(observed, value, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(current) => observed = current,
                }
            }
        }
    }

    #[async_trait]
    impl SubagentRunExecutor for TrackingExecutor {
        async fn execute(
            &self,
            _base_config: Arc<Config>,
            _store: Arc<SessionStore>,
            run: SubagentRun,
            _cancellation: CancellationToken,
        ) -> Result<String> {
            let global_now = self.active_global.fetch_add(1, Ordering::AcqRel) + 1;
            Self::bump_max(&self.max_global, global_now);

            {
                let mut active = self.active_per_session.lock();
                let per_session_now = {
                    let entry = active.entry(run.subagent_session_id.clone()).or_insert(0);
                    *entry += 1;
                    *entry
                };

                let mut max_map = self.max_per_session.lock();
                let entry = max_map.entry(run.subagent_session_id.clone()).or_insert(0);
                *entry = (*entry).max(per_session_now);
            }

            sleep(self.delay).await;

            {
                let mut active = self.active_per_session.lock();
                if let Some(entry) = active.get_mut(run.subagent_session_id.as_str()) {
                    *entry = entry.saturating_sub(1);
                }
            }

            self.active_global.fetch_sub(1, Ordering::AcqRel);
            Ok(serde_json::json!({"text":"tracked"}).to_string())
        }
    }

    async fn wait_for_status(
        runtime: &SubagentRuntime,
        run_id: &str,
        target_status: &str,
    ) -> SubagentRun {
        let start = Instant::now();
        loop {
            let run = runtime
                .poll_run(run_id)
                .await
                .unwrap()
                .expect("run should exist");
            if run.status == target_status {
                return run;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out waiting for status"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn subagent_runtime_transitions_run_queued_running_succeeded() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let runtime = SubagentRuntime::new(store);

        let queued = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "run scaffold".to_string(),
                input_json: Some(r#"{"task":"unit-test"}"#.to_string()),
                session_meta_json: None,
            })
            .await
            .unwrap();

        assert_eq!(queued.status, SubagentRunStatus::Queued.as_str());

        let completed = runtime
            .process_next_run_with(|_run| Ok(r#"{"text":"ok"}"#.to_string()))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(completed.run_id, queued.run_id);
        assert_eq!(completed.status, SubagentRunStatus::Succeeded.as_str());
        assert_eq!(completed.output_json.as_deref(), Some(r#"{"text":"ok"}"#));
        assert!(completed.started_at.is_some());
        assert!(completed.finished_at.is_some());
    }

    #[tokio::test]
    async fn subagent_runtime_stop_returns_none_for_missing_run() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let runtime = SubagentRuntime::new(store);

        let stopped = runtime.stop_run("missing-run-id").await.unwrap();
        assert!(stopped.is_none());
    }

    #[tokio::test]
    async fn subagent_runtime_cancel_before_start_marks_queued_run_canceled() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let (started_tx, started_rx) = oneshot::channel();
        let runtime = Arc::new(SubagentRuntime::with_executor(
            Arc::clone(&store),
            Arc::new(Config::default()),
            Arc::new(BlockingFirstExecutor {
                first_blocked: AtomicBool::new(true),
                started_tx: Mutex::new(Some(started_tx)),
                release_first: Arc::new(Notify::new()),
            }),
            DEFAULT_WORKER_POLL_INTERVAL,
        ));

        runtime.start_background_worker();

        let first = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "first".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();
        let second = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: Some(first.subagent_session_id.clone()),
                spec_id: None,
                prompt: "second".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();

        timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();

        let stopped = runtime
            .stop_run(second.run_id.as_str())
            .await
            .unwrap()
            .expect("stopped run should exist");
        assert_eq!(stopped.status, SubagentRunStatus::Canceled.as_str());
    }

    #[tokio::test]
    async fn subagent_runtime_recovery_requeues_and_executes_running_runs() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let runtime = Arc::new(SubagentRuntime::with_executor(
            Arc::clone(&store),
            Arc::new(Config::default()),
            Arc::new(ImmediateExecutor),
            Duration::from_millis(20),
        ));

        let queued = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "recover".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();
        let claimed = store.claim_next_queued_subagent_run().unwrap().unwrap();
        assert_eq!(claimed.run_id, queued.run_id);
        assert_eq!(claimed.status, SubagentRunStatus::Running.as_str());

        runtime.start_background_worker();

        let completed = wait_for_status(
            &runtime,
            queued.run_id.as_str(),
            SubagentRunStatus::Succeeded.as_str(),
        )
        .await;
        assert_eq!(completed.status, SubagentRunStatus::Succeeded.as_str());
    }

    #[tokio::test]
    async fn subagent_runtime_serializes_runs_within_session() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let tracking = Arc::new(TrackingExecutor {
            delay: Duration::from_millis(120),
            active_global: AtomicUsize::new(0),
            max_global: AtomicUsize::new(0),
            active_per_session: Mutex::new(HashMap::new()),
            max_per_session: Mutex::new(HashMap::new()),
        });
        let runtime = Arc::new(SubagentRuntime::with_executor(
            Arc::clone(&store),
            Arc::new(Config::default()),
            tracking.clone(),
            Duration::from_millis(10),
        ));
        runtime.start_background_worker();

        let first = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "one".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();
        let second = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: Some(first.subagent_session_id.clone()),
                spec_id: None,
                prompt: "two".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();

        let _ = wait_for_status(
            &runtime,
            first.run_id.as_str(),
            SubagentRunStatus::Succeeded.as_str(),
        )
        .await;
        let _ = wait_for_status(
            &runtime,
            second.run_id.as_str(),
            SubagentRunStatus::Succeeded.as_str(),
        )
        .await;

        let max_per_session = tracking.max_per_session.lock();
        assert_eq!(
            max_per_session.get(&first.subagent_session_id).copied(),
            Some(1)
        );
    }

    #[tokio::test]
    async fn subagent_runtime_runs_sessions_in_parallel() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let tracking = Arc::new(TrackingExecutor {
            delay: Duration::from_millis(200),
            active_global: AtomicUsize::new(0),
            max_global: AtomicUsize::new(0),
            active_per_session: Mutex::new(HashMap::new()),
            max_per_session: Mutex::new(HashMap::new()),
        });
        let runtime = Arc::new(SubagentRuntime::with_executor(
            Arc::clone(&store),
            Arc::new(Config::default()),
            tracking.clone(),
            Duration::from_millis(10),
        ));
        runtime.start_background_worker();

        let first = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "s1".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();
        let second = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "s2".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();

        let _ = wait_for_status(
            &runtime,
            first.run_id.as_str(),
            SubagentRunStatus::Succeeded.as_str(),
        )
        .await;
        let _ = wait_for_status(
            &runtime,
            second.run_id.as_str(),
            SubagentRunStatus::Succeeded.as_str(),
        )
        .await;

        assert!(
            tracking.max_global.load(Ordering::Acquire) >= 2,
            "expected parallel execution across sessions"
        );
    }
}
