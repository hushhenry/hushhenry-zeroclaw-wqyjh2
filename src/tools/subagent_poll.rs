use super::traits::{Tool, ToolResult};
use crate::session::SessionStore;
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

pub struct SubagentPollTool {
    workspace_dir: PathBuf,
}

impl SubagentPollTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for SubagentPollTool {
    fn name(&self) -> &str {
        "subagent_poll"
    }

    fn description(&self) -> &str {
        "Poll persisted subagent run state from memory/sessions.db"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "Subagent run identifier"
                }
            },
            "required": ["run_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = match args
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => value,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'run_id' parameter".to_string()),
                });
            }
        };

        let store = SessionStore::new(&self.workspace_dir)?;
        let Some(run) = store.get_subagent_run(run_id)? else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Subagent run not found: {run_id}")),
            });
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run.run_id,
                "subagent_session_id": run.subagent_session_id,
                "status": run.status,
                "prompt": run.prompt,
                "input_json": run.input_json,
                "output_json": run.output_json,
                "error_message": run.error_message,
                "queued_at": run.queued_at,
                "started_at": run.started_at,
                "finished_at": run.finished_at,
                "updated_at": run.updated_at
            }))?,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStore;
    use crate::subagent::{EnqueueSubagentRunRequest, SubagentRuntime};
    use tempfile::TempDir;

    #[tokio::test]
    async fn subagent_poll_returns_existing_run() {
        let workspace = TempDir::new().unwrap();
        let store = std::sync::Arc::new(SessionStore::new(workspace.path()).unwrap());
        let runtime = SubagentRuntime::new(store);

        let run = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "poll me".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .unwrap();

        let tool = SubagentPollTool::new(workspace.path().to_path_buf());
        let result = tool.execute(json!({ "run_id": run.run_id })).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("queued"));
    }

    #[tokio::test]
    async fn subagent_poll_returns_not_found_for_missing_run() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentPollTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({ "run_id": "missing-run-id" }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }
}
