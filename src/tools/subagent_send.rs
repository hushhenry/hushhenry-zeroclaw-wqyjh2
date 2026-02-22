use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::subagent::SubagentRuntime;
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
        "Create or reuse a subagent session for async delegation"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task prompt or context for the subagent session"
                },
                "subagent_session_id": {
                    "type": "string",
                    "description": "Existing subagent session id. If omitted, a new session is created."
                },
                "spec_name": {
                    "type": "string",
                    "description": "Optional subagent spec name (used when creating a new subagent session)"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Optional agent id (used when creating a new subagent session)"
                },
                "spec_id": {
                    "type": "string",
                    "description": "Deprecated alias for agent_id"
                },
                "input_json": {
                    "type": "string",
                    "description": "Optional JSON payload to attach to the session context"
                },
                "session_meta_json": {
                    "type": "string",
                    "description": "Optional JSON metadata stored on a newly created subagent session"
                },
                "parent_session_id": {
                    "type": "string",
                    "description": "Optional parent session id; stored in meta when creating a new session"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let _prompt = match args
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

        let agent_id = args
            .get("agent_id")
            .or(args.get("spec_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let _spec_name = args
            .get("spec_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let _input_json = args
            .get("input_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let session_meta_json = args
            .get("session_meta_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let parent_session_id = args
            .get("parent_session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let session_meta_json = if subagent_session_id.is_none() && parent_session_id.is_some() {
            let mut meta: serde_json::Map<String, serde_json::Value> = session_meta_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .and_then(|v: serde_json::Value| v.as_object().cloned())
                .unwrap_or_default();
            meta.insert(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.unwrap().to_string()),
            );
            Some(serde_json::to_string(&meta).unwrap_or_default())
        } else {
            session_meta_json
        };

        let runtime = SubagentRuntime::shared(Arc::clone(&self.config))?;
        let resolved_agent_id = if agent_id.is_some() || subagent_session_id.is_some() {
            agent_id
        } else if let Some(name) = _spec_name {
            match runtime.store.get_agent_by_name(name)? {
                Some(agent) => Some(agent.agent_id),
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Agent not found: {name}")),
                    });
                }
            }
        } else {
            None
        };

        let session = if let Some(id) = subagent_session_id {
            runtime
                .store
                .get_subagent_session(&id)?
                .ok_or_else(|| anyhow::anyhow!("Subagent session not found: {id}"))?
        } else {
            runtime.store.create_subagent_session(
                resolved_agent_id.as_deref(),
                session_meta_json.as_deref(),
            )?
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "subagent_session_id": session.subagent_session_id,
                "status": session.status,
                "agent_id": session.agent_id,
                "created_at": session.created_at,
                "updated_at": session.updated_at
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
    async fn subagent_send_creates_session() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentSendTool::new(Arc::new(config));

        let result = tool
            .execute(json!({"prompt": "execute task"}))
            .await
            .unwrap();

        assert!(result.success);
        let value: serde_json::Value = serde_json::from_str(result.output.as_str()).unwrap();
        let session_id = value
            .get("subagent_session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(!session_id.is_empty());
        assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("active"));

        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store.get_subagent_session(session_id).unwrap().unwrap();
        assert_eq!(session.status, "active");
    }
}
