use super::traits::{Tool, ToolResult};
use crate::session::SessionStore;
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 200;

pub struct SessionsListTool {
    workspace_dir: PathBuf,
}

impl SessionsListTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List sessions from memory/sessions.db, newest first"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_key": {
                    "type": "string",
                    "description": "Optional exact session_key filter"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum sessions to return (default: 20, max: 200)"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        let session_key = args
            .get("session_key")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let store = SessionStore::new(&self.workspace_dir)?;
        let sessions = store.list_sessions(session_key, limit)?;
        let output = sessions
            .iter()
            .map(|session| {
                json!({
                    "session_id": session.session_id,
                    "session_key": session.session_key,
                    "status": session.status,
                    "title": session.title,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "message_count": session.message_count
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)?,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionKey, SessionStore};
    use tempfile::TempDir;

    #[tokio::test]
    async fn sessions_list_returns_created_sessions() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_key = SessionKey::new("group:telegram:chat-1");
        let session_id = store.get_or_create_active(&session_key).unwrap();
        store
            .append_message(&session_id, "user", "hello", None)
            .unwrap();

        let tool = SessionsListTool::new(workspace.path().to_path_buf());
        let result = tool.execute(json!({ "limit": 5 })).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("group:telegram:chat-1"));
    }
}
