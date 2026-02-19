use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::session::{SessionStore, SubagentRunStatus};
use crate::subagent::{EnqueueSubagentRunRequest, SubagentRuntime};
use anyhow::Context;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_ONESHOT_TIMEOUT_SECS: u64 = 120;

pub struct SubagentSpawnOneshotTool {
    config: Arc<Config>,
}

impl SubagentSpawnOneshotTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    fn extract_text(output_json: Option<&str>) -> Option<String> {
        let raw = output_json?;
        let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
        parsed
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    }
}

#[async_trait]
impl Tool for SubagentSpawnOneshotTool {
    fn name(&self) -> &str {
        "subagent_spawn_oneshot"
    }

    fn description(&self) -> &str {
        "Spawn a subagent run and block until it reaches a terminal state (succeeded/failed/canceled); compatibility path for deprecated delegate-style workflows"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Task prompt for the subagent run"
                },
                "context": {
                    "type": "string",
                    "description": "Optional context prepended to the prompt"
                },
                "agent": {
                    "type": "string",
                    "description": "Legacy delegate agent name. Maps to a subagent spec name."
                },
                "spec_name": {
                    "type": "string",
                    "description": "Subagent spec name to use when creating a new subagent session"
                },
                "spec_id": {
                    "type": "string",
                    "description": "Subagent spec id to use when creating a new subagent session"
                },
                "subagent_session_id": {
                    "type": "string",
                    "description": "Existing subagent session id"
                },
                "input_json": {
                    "type": "string",
                    "description": "Optional JSON payload forwarded to the run"
                },
                "session_meta_json": {
                    "type": "string",
                    "description": "Optional JSON metadata stored on a newly created subagent session"
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum seconds to wait for terminal run completion (default: 120)"
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
            Some(value) => value,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'prompt' parameter".to_string()),
                });
            }
        };

        let context = args
            .get("context")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let full_prompt = match context {
            Some(ctx) => format!("[Context]\n{ctx}\n\n[Task]\n{prompt}"),
            None => prompt.to_string(),
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

        let legacy_agent = args
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let selected_spec_name = legacy_agent.or(spec_name);

        let input_json = args
            .get("input_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);

        let session_meta_json = args
            .get("session_meta_json")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_ONESHOT_TIMEOUT_SECS);

        if timeout_secs == 0 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'timeout_secs' must be >= 1".to_string()),
            });
        }

        let resolved_spec_id = if spec_id.is_some() || subagent_session_id.is_some() {
            spec_id
        } else if let Some(name) = selected_spec_name {
            let store = SessionStore::new(&self.config.workspace_dir)?;
            match store
                .get_subagent_spec_by_name(name)
                .with_context(|| format!("Failed to resolve subagent spec '{name}'"))?
            {
                Some(spec) => Some(spec.spec_id),
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Subagent spec not found: {name}. Configure a subagent spec, or migrate legacy config.agents first."
                        )),
                    });
                }
            }
        } else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Missing target: provide one of 'subagent_session_id', 'spec_id', 'spec_name', or legacy 'agent'".to_string(),
                ),
            });
        };

        let runtime = SubagentRuntime::shared(Arc::clone(&self.config))?;
        let run = runtime
            .enqueue_run(EnqueueSubagentRunRequest {
                subagent_session_id,
                spec_id: resolved_spec_id,
                prompt: full_prompt,
                input_json,
                session_meta_json,
            })?;

        let run = runtime
            .wait_for_run_completion(run.run_id.as_str(), Duration::from_secs(timeout_secs))
            .await?;

        let extracted_text = Self::extract_text(run.output_json.as_deref());
        let output = serde_json::to_string_pretty(&json!({
            "run_id": run.run_id,
            "subagent_session_id": run.subagent_session_id,
            "status": run.status,
            "output_json": run.output_json,
            "error_message": run.error_message,
            "text": extracted_text,
            "queued_at": run.queued_at,
            "started_at": run.started_at,
            "finished_at": run.finished_at,
            "updated_at": run.updated_at
        }))?;

        let status = SubagentRunStatus::from_str_opt(run.status.as_str());
        match status {
            Some(SubagentRunStatus::Succeeded) => Ok(ToolResult {
                success: true,
                output,
                error: None,
            }),
            Some(SubagentRunStatus::Failed | SubagentRunStatus::Canceled) => {
                Ok(ToolResult {
                    success: false,
                    output,
                    error: Some(run.error_message.unwrap_or_else(|| {
                        format!("Subagent run ended with status '{}'", run.status)
                    })),
                })
            }
            _ => Ok(ToolResult {
                success: false,
                output,
                error: Some(format!(
                    "Unexpected non-terminal subagent status '{}'",
                    run.status
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn subagent_spawn_oneshot_requires_prompt() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentSpawnOneshotTool::new(Arc::new(config));

        let result = tool.execute(json!({"spec_name": "any"})).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn subagent_spawn_oneshot_requires_target() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentSpawnOneshotTool::new(Arc::new(config));

        let result = tool.execute(json!({"prompt": "do work"})).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing target"));
    }

    #[tokio::test]
    async fn subagent_spawn_oneshot_agent_alias_uses_spec_name_lookup() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.default_provider = Some("not-a-provider".to_string());
        let config = Arc::new(config);

        let store = SessionStore::new(workspace.path()).unwrap();
        store
            .upsert_subagent_spec(
                "researcher",
                r#"{"provider":"not-a-provider","model":"dummy"}"#,
            )
            .unwrap();

        let tool = SubagentSpawnOneshotTool::new(Arc::clone(&config));
        let result = tool
            .execute(json!({
                "agent": "researcher",
                "prompt": "investigate",
                "timeout_secs": 3
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("\"status\""));
    }

    #[tokio::test]
    async fn subagent_spawn_oneshot_returns_not_found_for_missing_spec() {
        let workspace = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let tool = SubagentSpawnOneshotTool::new(Arc::new(config));

        let result = tool
            .execute(json!({
                "spec_name": "missing-spec",
                "prompt": "run",
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Subagent spec not found"));
    }
}
