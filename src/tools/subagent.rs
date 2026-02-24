//! Subagent tool: action-based API (spawn, send, stop) for subagent sessions.
//!
//! - **spawn**: create a subagent session (agent_id, optional parent_session_id for outbound).
//! - **send**: send a message **as inbound input** to a subagent session (the agent will process it).
//!   Contrast with the sessions tool's **reply** action, which delivers to the session's outbound.
//! - **stop**: send /stop to a subagent via internal channel.

use super::traits::{Tool, ToolResult};
use crate::channels::{build_internal_channel_message, dispatch_internal_message};
use crate::config::Config;
use crate::subagent::SubagentRuntime;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const ACTION_SPAWN: &str = "spawn";
const ACTION_SEND: &str = "send";
const ACTION_STOP: &str = "stop";

pub struct SubagentTool {
    config: Arc<Config>,
}

impl SubagentTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Subagent session control: spawn, send input to a subagent (inbound — agent processes it), or stop. Use sessions reply to send to a session's outbound (e.g. channel)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [ACTION_SPAWN, ACTION_SEND, ACTION_STOP],
                    "description": "Action: spawn (create session), send (message to session), stop (send /stop to session)"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Agent id for spawn (default: main)"
                },
                "outbound_key": {
                    "type": "string",
                    "description": "Optional outbound key for spawn (default: internal:{parent_session_id} when parent is set)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Subagent session id (for send and stop)"
                },
                "input": {
                    "type": "string",
                    "description": "Message content to send to subagent (for send)"
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
                    error: Some("Missing 'action' parameter (spawn | send | stop)".to_string()),
                });
            }
        };

        match action {
            ACTION_SPAWN => execute_spawn(Arc::clone(&self.config), args),
            ACTION_SEND => execute_send(Arc::clone(&self.config), args).await,
            ACTION_STOP => execute_stop(Arc::clone(&self.config), args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid action '{action}'; expected spawn, send, or stop"
                )),
            }),
        }
    }
}

fn execute_spawn(config: Arc<Config>, args: serde_json::Value) -> anyhow::Result<ToolResult> {
    let agent_id = args
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "main".to_string());

    let parent_session_id = args
        .get("parent_session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    let session_meta_json = parent_session_id
        .as_ref()
        .map(|pid| serde_json::json!({ "parent_session_id": pid }).to_string());

    let outbound_key = args
        .get("outbound_key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            parent_session_id
                .as_ref()
                .map(|pid| format!("internal:{}", pid))
        });

    let runtime = SubagentRuntime::shared(config)?;
    let session = runtime.store.create_subagent_session(
        Some(agent_id.as_str()),
        session_meta_json.as_deref(),
        outbound_key.as_deref(),
    )?;

    Ok(ToolResult {
        success: true,
        output: serde_json::to_string_pretty(&json!({
            "session_id": session.subagent_session_id,
            "agent_id": session.agent_id,
            "status": session.status,
            "created_at": session.created_at,
            "updated_at": session.updated_at
        }))?,
        error: None,
    })
}

async fn execute_send(config: Arc<Config>, args: serde_json::Value) -> anyhow::Result<ToolResult> {
    let session_id = match args
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s,
        None => {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'session_id' parameter for send".to_string()),
            });
        }
    };

    let input = match args.get("input").and_then(serde_json::Value::as_str) {
        Some(s) => s.trim(),
        None => {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'input' parameter for send".to_string()),
            });
        }
    };

    let runtime = SubagentRuntime::shared(config)?;
    let Some(session) = runtime.store.get_subagent_session(session_id)? else {
        return Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("Subagent session not found: {session_id}")),
        });
    };

    if session.status != "active" {
        return Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "Subagent session not active (status: {}); cannot send",
                session.status
            )),
        });
    }

    let msg = build_internal_channel_message("subagent_send", session_id, input);
    match dispatch_internal_message(msg).await {
        Ok(()) => Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "session_id": session_id,
                "status": "dispatched",
                "delivery": "internal"
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

async fn execute_stop(config: Arc<Config>, args: serde_json::Value) -> anyhow::Result<ToolResult> {
    let session_id = match args
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s,
        None => {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing 'session_id' parameter for stop".to_string()),
            });
        }
    };

    let runtime = SubagentRuntime::shared(config)?;
    let Some(session) = runtime.store.get_subagent_session(session_id)? else {
        return Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("Subagent session not found: {session_id}")),
        });
    };

    if session.status == "stopped" {
        return Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "session_id": session.subagent_session_id,
                "status": session.status,
                "message": "already stopped"
            }))?,
            error: None,
        });
    }

    let msg = build_internal_channel_message("subagent_stop", session_id, "/stop");
    let _ = dispatch_internal_message(msg).await;

    runtime.store.mark_subagent_session_stopped(session_id)?;
    let updated = runtime.store.get_subagent_session(session_id)?.unwrap();

    Ok(ToolResult {
        success: true,
        output: serde_json::to_string_pretty(&json!({
            "session_id": updated.subagent_session_id,
            "status": updated.status,
            "updated_at": updated.updated_at
        }))?,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStore;
    use tempfile::TempDir;

    fn test_config(workspace: &TempDir) -> Arc<Config> {
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        Arc::new(config)
    }

    #[tokio::test]
    async fn subagent_spawn_creates_session_and_returns_session_id() {
        let workspace = TempDir::new().unwrap();
        let config = test_config(&workspace);
        let tool = SubagentTool::new(config);

        let result = tool.execute(json!({"action": "spawn"})).await.unwrap();

        assert!(result.success, "spawn should succeed: {:?}", result.error);
        let out: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let session_id = out.get("session_id").and_then(|v| v.as_str()).unwrap();
        assert!(!session_id.is_empty());
        assert_eq!(out.get("agent_id").and_then(|v| v.as_str()), Some("main"));
        assert_eq!(out.get("status").and_then(|v| v.as_str()), Some("active"));

        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.get_subagent_session(session_id).unwrap().unwrap();
        assert_eq!(session.agent_id, "main");
        assert_eq!(session.status, "active");
    }

    #[tokio::test]
    async fn subagent_spawn_with_agent_id() {
        let workspace = TempDir::new().unwrap();
        let config = test_config(&workspace);
        let tool = SubagentTool::new(config);

        let result = tool
            .execute(json!({"action": "spawn", "agent_id": "worker"}))
            .await
            .unwrap();

        assert!(result.success);
        let out: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out.get("agent_id").and_then(|v| v.as_str()), Some("worker"));

        let session_id = out.get("session_id").and_then(|v| v.as_str()).unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.get_subagent_session(session_id).unwrap().unwrap();
        assert_eq!(session.agent_id, "worker");
    }

    #[tokio::test]
    async fn subagent_spawn_with_parent_session_id_sets_meta() {
        let workspace = TempDir::new().unwrap();
        let config = test_config(&workspace);
        let tool = SubagentTool::new(config);

        let result = tool
            .execute(json!({
                "action": "spawn",
                "agent_id": "main",
                "parent_session_id": "parent-123"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let out: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let session_id = out.get("session_id").and_then(|v| v.as_str()).unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.get_subagent_session(session_id).unwrap().unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(session.meta_json.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            meta.get("parent_session_id").and_then(|v| v.as_str()),
            Some("parent-123")
        );
    }

    #[tokio::test]
    async fn subagent_send_requires_session_id_and_input() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentTool::new(test_config(&workspace));

        let r = tool.execute(json!({"action": "send"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("session_id"));

        let r = tool
            .execute(json!({"action": "send", "session_id": "sid"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("input"));
    }

    #[tokio::test]
    async fn subagent_send_rejects_nonexistent_session() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentTool::new(test_config(&workspace));

        let result = tool
            .execute(json!({
                "action": "send",
                "session_id": "nonexistent-session-id",
                "input": "hello"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn subagent_send_dispatches_when_session_exists() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.create_subagent_session(None, None, None).unwrap();

        let tool = SubagentTool::new(test_config(&workspace));
        let result = tool
            .execute(json!({
                "action": "send",
                "session_id": session.subagent_session_id,
                "input": "task content"
            }))
            .await
            .unwrap();

        // Dispatch may fail in unit test (no channel dispatcher); we only assert tool accepts valid session.
        if result.success {
            let out: serde_json::Value = serde_json::from_str(&result.output).unwrap();
            assert_eq!(
                out.get("delivery").and_then(|v| v.as_str()),
                Some("internal")
            );
        } else {
            assert!(
                result
                    .error
                    .as_ref()
                    .map(|e| e.contains("dispatcher") || e.contains("channel"))
                    .unwrap_or(false),
                "expected dispatcher/channel error when no daemon: {:?}",
                result.error
            );
        }
    }

    #[tokio::test]
    async fn subagent_stop_requires_session_id() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentTool::new(test_config(&workspace));

        let result = tool.execute(json!({"action": "stop"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("session_id"));
    }

    #[tokio::test]
    async fn subagent_stop_returns_not_found_for_missing_session() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentTool::new(test_config(&workspace));

        let result = tool
            .execute(json!({"action": "stop", "session_id": "missing-session-id"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn subagent_stop_marks_session_stopped() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.create_subagent_session(None, None, None).unwrap();
        assert_eq!(session.status, "active");

        let tool = SubagentTool::new(test_config(&workspace));
        let result = tool
            .execute(json!({
                "action": "stop",
                "session_id": session.subagent_session_id
            }))
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

    #[tokio::test]
    async fn subagent_stop_idempotent_when_already_stopped() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.create_subagent_session(None, None, None).unwrap();
        store
            .mark_subagent_session_stopped(session.subagent_session_id.as_str())
            .unwrap();

        let tool = SubagentTool::new(test_config(&workspace));
        let result = tool
            .execute(json!({
                "action": "stop",
                "session_id": session.subagent_session_id
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("already stopped"));
    }

    #[tokio::test]
    async fn subagent_missing_action_fails() {
        let workspace = TempDir::new().unwrap();
        let tool = SubagentTool::new(test_config(&workspace));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("action"));

        let result = tool.execute(json!({"action": "invalid"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid action"));
    }
}
