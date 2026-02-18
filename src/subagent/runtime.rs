use crate::session::{SessionStore, SubagentRun};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

const DEFAULT_WORKER_OUTPUT: &str = "subagent run completed (scaffold)";

#[derive(Debug, Clone)]
pub struct EnqueueSubagentRunRequest {
    pub subagent_session_id: Option<String>,
    pub spec_id: Option<String>,
    pub prompt: String,
    pub input_json: Option<String>,
    pub session_meta_json: Option<String>,
}

pub struct SubagentRuntime {
    store: Arc<SessionStore>,
}

impl SubagentRuntime {
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self { store }
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

        self.store.enqueue_subagent_run(
            &subagent_session_id,
            &request.prompt,
            request.input_json.as_deref(),
        )
    }

    pub async fn process_next_run(&self) -> Result<Option<SubagentRun>> {
        self.process_next_run_with(|_run| Ok(DEFAULT_WORKER_OUTPUT.to_string()))
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
}

#[cfg(test)]
mod tests {
    use super::{EnqueueSubagentRunRequest, SubagentRuntime};
    use crate::session::{SessionStore, SubagentRunStatus};
    use std::sync::Arc;
    use tempfile::TempDir;

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
}
