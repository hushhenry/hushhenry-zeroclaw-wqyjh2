use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::subagent::SubagentRuntime;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct SubagentStopTool {
    config: Arc<Config>,
}

impl SubagentStopTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for SubagentStopTool {
    fn name(&self) -> &str {
        "subagent_stop"
    }

    fn description(&self) -> &str {
        "Cancel a queued or running subagent run"
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

        let runtime = SubagentRuntime::shared(Arc::clone(&self.config))?;
        let Some(run) = runtime.stop_run(run_id).await? else {
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
    use crate::subagent::EnqueueSubagentRunRequest;
    use tempfile::TempDir;

    #[tokio::test]
    async fn subagent_stop_returns_not_found_for_missing_run() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentStopTool::new(Arc::new(config));

        let result = tool.execute(json!({"run_id":"missing-run"})).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn subagent_stop_cancels_queued_run() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let config = Arc::new(config);
        let runtime = SubagentRuntime::shared(Arc::clone(&config)).unwrap();

        let run = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id: None,
                spec_id: None,
                prompt: "cancel me".to_string(),
                input_json: None,
                session_meta_json: None,
            })
            .await
            .unwrap();

        let tool = SubagentStopTool::new(config);
        let result = tool.execute(json!({"run_id":run.run_id})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("canceled"));
    }
}
