use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::subagent::{EnqueueSubagentRunRequest, SubagentRuntime};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct SubagentSendTool {
    config: Arc<Config>,
}

impl SubagentSendTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for SubagentSendTool {
    fn name(&self) -> &str {
        "subagent_send"
    }

    fn description(&self) -> &str {
        "Create a subagent run and enqueue it for async execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task prompt for the subagent run"
                },
                "subagent_session_id": {
                    "type": "string",
                    "description": "Existing subagent session id. If omitted, a new session is created."
                },
                "spec_name": {
                    "type": "string",
                    "description": "Optional subagent spec name (used when creating a new subagent session)"
                },
                "spec_id": {
                    "type": "string",
                    "description": "Optional subagent spec id (used when creating a new subagent session)"
                },
                "input_json": {
                    "type": "string",
                    "description": "Optional JSON payload forwarded to the run"
                },
                "session_meta_json": {
                    "type": "string",
                    "description": "Optional JSON metadata stored on a newly created subagent session"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let prompt = match args
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => value.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'prompt' parameter".to_string()),
                });
            }
        };

        let subagent_session_id = args
            .get("subagent_session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        let spec_id = args
            .get("spec_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let spec_name = args
            .get("spec_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let input_json = args
            .get("input_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let session_meta_json = args
            .get("session_meta_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);

        let runtime = SubagentRuntime::shared(Arc::clone(&self.config))?;
        let resolved_spec_id = if spec_id.is_some() || subagent_session_id.is_some() {
            spec_id
        } else if let Some(name) = spec_name {
            let store = crate::session::SessionStore::new(&self.config.workspace_dir)?;
            match store.get_subagent_spec_by_name(name)? {
                Some(spec) => Some(spec.spec_id),
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Subagent spec not found: {name}")),
                    });
                }
            }
        } else {
            None
        };

        let run = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id,
                spec_id: resolved_spec_id,
                prompt,
                input_json,
                session_meta_json,
            })?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run.run_id,
                "subagent_session_id": run.subagent_session_id,
                "status": run.status,
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
    use tempfile::TempDir;

    #[tokio::test]
    async fn subagent_send_enqueues_run() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentSendTool::new(Arc::new(config));

        let result = tool
            .execute(json!({"prompt":"execute task"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("queued"));

        let store = SessionStore::new(workspace.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(result.output.as_str()).unwrap();
        let run_id = value
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        let run = store.get_subagent_run(run_id).unwrap().unwrap();
        assert_eq!(run.status, "queued");
    }
}
