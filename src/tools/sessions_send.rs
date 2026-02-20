use super::traits::{Tool, ToolResult};
use crate::channels::{build_internal_channel_message, dispatch_internal_message};
use crate::session::{SessionId, SessionStore};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

pub struct SessionsSendTool {
    workspace_dir: PathBuf,
}

impl SessionsSendTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to a session (by session_id from sessions_list). The session will process and respond."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session identifier from sessions_list"
                },
                "content": {
                    "type": "string",
                    "description": "Message content to send"
                }
            },
            "required": ["session_id", "content"]
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

        let content = match args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) => value,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'content' parameter".to_string()),
                });
            }
        };

        let store = SessionStore::new(&self.workspace_dir)?;
        if !store.session_exists(&session_id)? {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Session not found: {}", session_id.as_str())),
            });
        }

        // Deliver only via internal channel; session is scheduled and will respond. No session DB write.
        let msg = build_internal_channel_message(
            "sessions_send",
            session_id.as_str(),
            content,
            None::<&str>,
        );
        match dispatch_internal_message(msg).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "session_id": session_id.as_str(),
                    "status": "dispatched",
                    "delivery": "internal_channel"
                }))?,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionKey, SessionStore};
    use tempfile::TempDir;

    #[tokio::test]
    async fn sessions_send_requires_dispatcher_running() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_id = store
            .get_or_create_active(&SessionKey::new("group:discord:chan-1"))
            .unwrap();

        let tool = SessionsSendTool::new(workspace.path().to_path_buf());
        // No channel dispatcher in unit test: dispatch fails, tool returns error.
        let result = tool
            .execute(json!({
                "session_id": session_id.as_str(),
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap_or(&String::new()).contains("dispatcher")
                || result.error.as_ref().unwrap_or(&String::new()).contains("channel"),
            "expected error about dispatcher/channel, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn sessions_send_rejects_nonexistent_session() {
        let workspace = TempDir::new().unwrap();

        let tool = SessionsSendTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({
                "session_id": "nonexistent-session-id",
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Session not found"));
    }
}
