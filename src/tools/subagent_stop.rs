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
        "Stop a subagent session so it no longer receives or processes messages"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "subagent_session_id": {
                    "type": "string",
                    "description": "Subagent session to stop"
                }
            },
            "required": ["subagent_session_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let subagent_session_id = match args
            .get("subagent_session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(id) => id,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'subagent_session_id' parameter".to_string()),
                });
            }
        };

        let runtime = SubagentRuntime::shared(Arc::clone(&self.config))?;
        let Some(session) = runtime.store.get_subagent_session(subagent_session_id)? else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Subagent session not found: {subagent_session_id}")),
            });
        };

        if session.status == "stopped" {
            return Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "subagent_session_id": session.subagent_session_id,
                    "status": session.status,
                    "message": "already stopped"
                }))?,
                error: None,
            });
        }

        runtime.store.mark_subagent_session_stopped(subagent_session_id)?;
        let updated = runtime
            .store
            .get_subagent_session(subagent_session_id)?
            .unwrap();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "subagent_session_id": updated.subagent_session_id,
                "status": updated.status,
                "updated_at": updated.updated_at
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
    async fn subagent_stop_returns_not_found_for_missing_session() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentStopTool::new(Arc::new(config));

        let result = tool
            .execute(json!({"subagent_session_id": "missing-session-id"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn subagent_stop_marks_session_stopped() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.create_subagent_session(None, None).unwrap();
        assert_eq!(session.status, "active");

        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentStopTool::new(Arc::new(config));
        let result = tool
            .execute(json!({"subagent_session_id": session.subagent_session_id}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("stopped"));

        let updated = store
            .get_subagent_session(session.subagent_session_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "stopped");
    }
}
