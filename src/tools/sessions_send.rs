use super::traits::{Tool, ToolResult};
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
        "Append a message to a specific session_id in memory/sessions.db"
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
                    "description": "Message content to append"
                },
                "role": {
                    "type": "string",
                    "enum": ["user", "assistant", "tool"],
                    "description": "Message role (default: user)"
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

        let role = match args
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("user")
        {
            "user" | "assistant" | "tool" => args
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("user"),
            invalid => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid 'role': {invalid}. Expected one of: user, assistant, tool"
                    )),
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

        store.append_message(&session_id, role, content, None)?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "session_id": session_id.as_str(),
                "role": role,
                "status": "appended"
            }))?,
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
    async fn sessions_send_appends_message_to_existing_session() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_id = store
            .get_or_create_active(&SessionKey::new("group:discord:chan-1"))
            .unwrap();

        let tool = SessionsSendTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({
                "session_id": session_id.as_str(),
                "content": "tool-inserted",
                "role": "assistant"
            }))
            .await
            .unwrap();
        assert!(result.success);

        let messages = store.load_recent_messages(&session_id, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "tool-inserted");
        assert_eq!(messages[0].role, "assistant");
    }

    #[tokio::test]
    async fn sessions_send_rejects_unknown_role() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_id = store
            .get_or_create_active(&SessionKey::new("group:discord:chan-2"))
            .unwrap();

        let tool = SessionsSendTool::new(workspace.path().to_path_buf());
        let result = tool
            .execute(json!({
                "session_id": session_id.as_str(),
                "content": "bad-role",
                "role": "system"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Invalid 'role'"));
    }
}
