use super::traits::{Tool, ToolResult};
use crate::channels::INTERNAL_MESSAGE_CHANNEL;
use crate::config::Config;
use crate::session::{SessionKey, SessionRouteMetadata, SessionStore};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct SubagentSpawnTool {
    config: Arc<Config>,
}

impl SubagentSpawnTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for SubagentSpawnTool {
    fn name(&self) -> &str {
        "subagent_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a subagent as a new child session. Returns child session information."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Task/prompt for the subagent"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Optional agent id or name from AgentSpec registry"
                },
                "label": {
                    "type": "string",
                    "description": "Optional human-readable label for the subagent task"
                },
                "source_session_id": {
                    "type": "string",
                    "description": "Parent session id (passed automatically by runtime)"
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let agent_id = args.get("agent_id").and_then(|v| v.as_str());
        let label = args.get("label").and_then(|v| v.as_str()).unwrap_or("subagent task");
        let parent_session_id = args.get("source_session_id").and_then(|v| v.as_str());

        let store = SessionStore::new(&self.config.workspace_dir)?;
        
        // Create a unique session key for the child. 
        // For simplicity, we use a random key prefixed with 'subagent:'
        let child_key = SessionKey::new(format!("subagent:{}", uuid::Uuid::new_v4()));
        let child_session_id = store.create_new(&child_key)?;

        // Link to parent context (conceptual, using route metadata)
        let metadata = SessionRouteMetadata {
            agent_id: agent_id.map(|s| s.to_string()),
            channel: INTERNAL_MESSAGE_CHANNEL.to_string(),
            account_id: None,
            chat_type: "direct".to_string(),
            chat_id: child_key.as_str().to_string(),
            route_id: None,
            sender_id: parent_session_id.unwrap_or("parent").to_string(),
            title: Some(label.to_string()),
            deliver: false,
        };
        store.upsert_route_metadata(&child_session_id, &metadata)?;

        if let Some(aid) = agent_id {
            if let Ok(Some(spec)) = store.resolve_agent_spec(aid) {
                store.set_active_agent_id(&child_session_id, spec.agent_id.as_str())?;
            }
        }

        // Create a subagent session record to store the parent link
        let meta_json = parent_session_id.map(|id| json!({"parent_session_id": id}).to_string());
        store.create_subagent_session_with_id(child_key.as_str(), agent_id, meta_json.as_deref())?;

        // Inject the task as the first user message
        store.append_message(&child_session_id, "user", task, None)?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "session_id": child_session_id.as_str(),
                "session_key": child_key.as_str(),
                "label": label,
                "status": "spawned"
            }))?,
            error: None,
        })
    }
}
