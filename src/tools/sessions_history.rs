use super::traits::{Tool, ToolResult};
use crate::session::{SessionId, SessionStore};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;

pub struct SessionsHistoryTool {
    workspace_dir: PathBuf,
}

impl SessionsHistoryTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Load recent message history for a specific session_id from memory/sessions.db"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session identifier from sessions_list"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum messages to return (default: 50, max: 500)"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let session_id = match args
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => SessionId::from_string(value.to_owned()),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'session_id' parameter".to_string()),
                });
            }
        };

        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        let store = SessionStore::new(&self.workspace_dir)?;
        if !store.session_exists(&session_id)? {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Session not found: {}", session_id.as_str())),
            });
        }

        let messages = store.load_recent_messages(&session_id, limit)?;
        let output = messages
            .iter()
            .map(|message| {
                json!({
                    "id": message.id,
                    "role": message.role,
                    "content": message.content,
                    "created_at": message.created_at,
                    "meta_json": message.meta_json
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "session_id": session_id.as_str(),
                "messages": output
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
    async fn sessions_history_returns_messages_for_session_id() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_id = store
            .get_or_create_active("channel:telegram:zeroclaw_user")
            .unwrap();
        store
            .append_message(&session_id, "user", "history-message", None)
            .unwrap();

        let tool = SessionsHistoryTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({ "session_id": session_id.as_str(), "limit": 5 }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("history-message"));
    }

    #[tokio::test]
    async fn sessions_history_returns_error_for_unknown_session() {
        let workspace = TempDir::new().unwrap();
        let tool = SessionsHistoryTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({ "session_id": "missing-session-id" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Session not found"));
    }
}
