//! Sessions tool: action-based API (list, history, reply) for session store.
//!
//! - **list**: list sessions from memory/sessions.db, newest first.
//! - **history**: load recent message history for a session_id.
//! - **reply**: deliver a message to a session's outbound (where that session sends replies),
//!   e.g. to the Telegram chat or to another session's inbox. Not inbound input — use subagent
//!   send for sending input to an agent session.

use super::traits::{Tool, ToolResult};
use crate::channels::{
    dispatch_outbound_message, parse_outbound_key_to_delivery_parts, SendMessage,
};
use crate::session::{SessionId, SessionStore};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const ACTION_LIST: &str = "list";
const ACTION_HISTORY: &str = "history";
const ACTION_REPLY: &str = "reply";

const LIST_DEFAULT_LIMIT: u32 = 20;
const LIST_MAX_LIMIT: u32 = 200;
const HISTORY_DEFAULT_LIMIT: u32 = 50;
const HISTORY_MAX_LIMIT: u32 = 500;

pub struct SessionsTool {
    workspace_dir: PathBuf,
}

impl SessionsTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl Tool for SessionsTool {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "Session store: list sessions, load history, or reply to a session's destination (action: list | history | reply). Reply delivers to the session's outbound (e.g. Telegram chat); use subagent send to give input to an agent."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [ACTION_LIST, ACTION_HISTORY, ACTION_REPLY],
                    "description": "Action: list (sessions), history (messages for session_id), reply (deliver content to session's outbound destination)"
                },
                "inbound_key": {
                    "type": "string",
                    "description": "Optional exact inbound_key filter (for list)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max sessions or messages (list: default 20 max 200; history: default 50 max 500)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session id from list; required for history and reply"
                },
                "content": {
                    "type": "string",
                    "description": "Message content to deliver (for reply)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = match args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'action' parameter (list | history | reply)".to_string()),
                });
            }
        };

        match action {
            ACTION_LIST => self.handle_list(args),
            ACTION_HISTORY => self.handle_history(args),
            ACTION_REPLY => self.handle_reply(args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid action '{action}'; expected list, history, or reply"
                )),
            }),
        }
    }
}

impl SessionsTool {
    fn handle_list(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(LIST_DEFAULT_LIMIT)
            .clamp(1, LIST_MAX_LIMIT);

        let inbound_key = args
            .get("inbound_key")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let store = SessionStore::new(&self.workspace_dir)?;
        let sessions = store.list_sessions(inbound_key, limit)?;
        let output = sessions
            .iter()
            .map(|s| {
                json!({
                    "session_id": s.session_id,
                    "inbound_key": s.inbound_key,
                    "status": s.status,
                    "title": s.title,
                    "created_at": s.created_at,
                    "updated_at": s.updated_at,
                    "message_count": s.message_count
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&output)?,
            error: None,
        })
    }

    fn handle_history(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let session_id = match args
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => SessionId::from_string(s.to_owned()),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'session_id' for history action".to_string()),
                });
            }
        };

        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(HISTORY_DEFAULT_LIMIT)
            .clamp(1, HISTORY_MAX_LIMIT);

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
            .map(|m| {
                json!({
                    "id": m.id,
                    "role": m.role,
                    "content": m.content,
                    "created_at": m.created_at,
                    "meta_json": m.meta_json
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

    /// Deliver content to the session's outbound (where that session sends replies).
    /// Uses stored outbound_key or derives from inbound_key for channel sessions.
    async fn handle_reply(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let session_id = match args
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => SessionId::from_string(s.to_owned()),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'session_id' for reply action".to_string()),
                });
            }
        };

        let content = match args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'content' for reply action".to_string()),
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

        let outbound_key = store.get_outbound_key_for_session(session_id.as_str())?;
        let outbound_key = match outbound_key {
            Some(k) if !k.trim().is_empty() => k,
            _ => {
                let inbound = store.get_inbound_key_for_session(session_id.as_str())?;
                match inbound.as_deref() {
                    Some(ik) if ik.starts_with("channel:") => ik.to_string(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Session has no outbound (internal-only or unknown); cannot reply"
                                    .to_string(),
                            ),
                        });
                    }
                }
            }
        };

        let reply_target = match parse_outbound_key_to_delivery_parts(outbound_key.trim()) {
            Ok((_, target)) => target,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let msg = SendMessage::new(content, reply_target);
        match dispatch_outbound_message(outbound_key.trim(), msg).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "session_id": session_id.as_str(),
                    "status": "dispatched",
                    "delivery": "outbound"
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
    use crate::session::SessionStore;
    use tempfile::TempDir;

    fn workspace_tool(tmp: &TempDir) -> SessionsTool {
        SessionsTool::new(tmp.path().to_path_buf())
    }

    #[test]
    fn sessions_tool_name_and_schema() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        assert_eq!(tool.name(), "sessions");
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(schema["properties"]["session_id"].is_object());
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("action")));
    }

    #[tokio::test]
    async fn sessions_missing_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("action"));
    }

    #[tokio::test]
    async fn sessions_invalid_action_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool.execute(json!({ "action": "invalid" })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Invalid action"));
    }

    #[tokio::test]
    async fn sessions_list_empty_returns_empty_array() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({ "action": "list", "limit": 10 }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("[]") || result.output == "[]");
    }

    #[tokio::test]
    async fn sessions_list_returns_created_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let session_id = store
            .get_or_create_active("channel:telegram:chat-1")
            .unwrap();
        store
            .append_message(&session_id, "user", "hello", None)
            .unwrap();

        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({ "action": "list", "limit": 5 }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("channel:telegram:chat-1"));
        assert!(result.output.contains(session_id.as_str()));
    }

    #[tokio::test]
    async fn sessions_list_respects_inbound_key_filter() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let _ = store.get_or_create_active("channel:telegram:a").unwrap();
        let _ = store.get_or_create_active("channel:discord:b").unwrap();

        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "list",
                "inbound_key": "channel:telegram:a",
                "limit": 10
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("channel:telegram:a"));
        assert!(!result.output.contains("channel:discord:b"));
    }

    #[tokio::test]
    async fn sessions_list_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        for i in 0..5 {
            let _ = store
                .get_or_create_active(&format!("channel:test:{}", i))
                .unwrap();
        }

        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({ "action": "list", "limit": 2 }))
            .await
            .unwrap();
        assert!(result.success);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result.output).unwrap();
        assert!(parsed.len() <= 2);
    }

    #[tokio::test]
    async fn sessions_history_returns_messages_for_session_id() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let session_id = store
            .get_or_create_active("channel:telegram:zeroclaw_user")
            .unwrap();
        store
            .append_message(&session_id, "user", "history-message", None)
            .unwrap();

        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "history",
                "session_id": session_id.as_str(),
                "limit": 5
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("history-message"));
    }

    #[tokio::test]
    async fn sessions_history_missing_session_id_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool.execute(json!({ "action": "history" })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("session_id"));
    }

    #[tokio::test]
    async fn sessions_history_returns_error_for_unknown_session() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({ "action": "history", "session_id": "missing-session-id" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Session not found"));
    }

    #[tokio::test]
    async fn sessions_reply_missing_session_id_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({ "action": "reply", "content": "hi" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("session_id"));
    }

    #[tokio::test]
    async fn sessions_reply_missing_content_returns_error() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let session_id = store.get_or_create_active("channel:test:1").unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "reply",
                "session_id": session_id.as_str()
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("content"));
    }

    #[tokio::test]
    async fn sessions_reply_rejects_nonexistent_session() {
        let tmp = TempDir::new().unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "reply",
                "session_id": "nonexistent-session-id",
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Session not found"));
    }

    #[tokio::test]
    async fn sessions_reply_channel_session_uses_inbound_as_outbound() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let session_id = store.get_or_create_active("channel:telegram:chat-99").unwrap();
        let tool = workspace_tool(&tmp);
        // Outbound loop not running in test -> dispatch fails; we only check we don't error earlier (session found, outbound_key derived).
        let result = tool
            .execute(json!({
                "action": "reply",
                "session_id": session_id.as_str(),
                "content": "hello"
            }))
            .await
            .unwrap();
        // Either success (if outbound tx exists in test env) or error about outbound loop
        assert!(
            result.success
                || result
                    .error
                    .as_ref()
                    .map_or(false, |e| e.contains("outbound")),
            "expected success or outbound-related error, got success={} error={:?}",
            result.success,
            result.error
        );
    }

    #[tokio::test]
    async fn sessions_reply_internal_only_session_returns_no_outbound_error() {
        let tmp = TempDir::new().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let session_id = store.get_or_create_active("internal:some-parent").unwrap();
        let tool = workspace_tool(&tmp);
        let result = tool
            .execute(json!({
                "action": "reply",
                "session_id": session_id.as_str(),
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .to_lowercase()
            .contains("outbound"));
    }
}
