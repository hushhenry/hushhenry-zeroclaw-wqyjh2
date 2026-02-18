use super::shell_exec_runtime::{
    LogExecRunResult, PollExecRunResponse, ShellExecRuntime, SpawnExecRunRequest,
};
use super::traits::{Tool, ToolResult};
use crate::runtime::RuntimeAdapter;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Maximum shell command execution time before kill.
const SHELL_TIMEOUT_SECS: u64 = 60;
/// Maximum output size in bytes (1MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const MAX_WATCHERS: usize = 16;
const MAX_WATCH_REGEX_LEN: usize = 256;
/// Environment variables safe to pass to shell commands.
/// Only functional variables are included — never API keys or secrets.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

#[derive(Debug, Deserialize, Serialize)]
struct ShellWatchSpec {
    regex: String,
    event: String,
    #[serde(default)]
    once: Option<bool>,
    #[serde(default)]
    scope: Option<String>,
}

/// Shell command execution tool with sandboxing
pub struct ShellTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
}

impl ShellTool {
    pub fn new(security: Arc<SecurityPolicy>, runtime: Arc<dyn RuntimeAdapter>) -> Self {
        Self { security, runtime }
    }

    fn validate_command_access(&self, command: &str, approved: bool) -> Option<ToolResult> {
        if self.security.is_rate_limited() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        if let Err(reason) = self.security.validate_command_execution(command, approved) {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(reason),
            });
        }

        if !self.security.record_action() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        None
    }

    async fn execute_legacy(&self, command: &str, approved: bool) -> anyhow::Result<ToolResult> {
        if let Some(result) = self.validate_command_access(command, approved) {
            return Ok(result);
        }

        // Execute with timeout to prevent hanging commands.
        // Clear the environment to prevent leaking API keys and other secrets
        // (CWE-200), then re-add only safe, functional variables.
        let mut cmd = match self
            .runtime
            .build_shell_command(command, &self.security.workspace_dir)
        {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build runtime command: {e}")),
                });
            }
        };
        cmd.env_clear();

        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }

        let result =
            tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Truncate output to prevent OOM
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(stdout.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stdout.push_str("\n... [output truncated at 1MB]");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    stderr.truncate(stderr.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stderr.push_str("\n... [stderr truncated at 1MB]");
                }

                Ok(ToolResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SHELL_TIMEOUT_SECS}s and was killed"
                )),
            }),
        }
    }

    async fn execute_spawn(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let Some(result) = self.validate_command_access(command, approved) {
            return Ok(result);
        }

        let watch_specs = parse_watch_specs(args.get("watch"))?;
        let watch_json = if watch_specs.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&watch_specs)?)
        };

        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("cli")
            .to_string();
        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let pty = args.get("pty").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(SHELL_TIMEOUT_SECS) as i64;
        let max_output_bytes = args
            .get("max_output_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_OUTPUT_BYTES as u64) as i64;

        let exec_runtime = ShellExecRuntime::shared(self.security.clone(), self.runtime.clone())?;
        let run = exec_runtime
            .enqueue_run(SpawnExecRunRequest {
                session_id: session_id.clone(),
                command: command.to_string(),
                pty,
                timeout_secs,
                max_output_bytes,
                watch_json,
            })
            .await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run.run_id,
                "session_id": run.session_id,
                "status": run.status,
                "queued_at": run.queued_at,
                "background": background,
                "pty": pty,
                "note": if background {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String("background=false is accepted, but runs are still processed asynchronously in phase 1".to_string())
                }
            }))?,
            error: None,
        })
    }

    async fn execute_poll(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = args
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter"))?;
        let since_seq = args.get("since_seq").and_then(|v| v.as_i64());

        let exec_runtime = ShellExecRuntime::shared(self.security.clone(), self.runtime.clone())?;
        let Some(PollExecRunResponse {
            run,
            items,
            next_seq,
            snippet,
        }) = exec_runtime.poll_run(run_id, since_seq).await?
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Exec run not found: {run_id}")),
            });
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run.run_id,
                "session_id": run.session_id,
                "status": run.status,
                "command": run.command,
                "exit_code": run.exit_code,
                "output_bytes": run.output_bytes,
                "truncated": run.truncated,
                "error_message": run.error_message,
                "queued_at": run.queued_at,
                "started_at": run.started_at,
                "finished_at": run.finished_at,
                "updated_at": run.updated_at,
                "since_seq": since_seq,
                "next_seq": next_seq,
                "snippet": snippet,
                "items": items.iter().map(|item| json!({
                    "seq": item.seq,
                    "type": item.item_type,
                    "payload": item.payload,
                    "meta_json": item.meta_json,
                    "created_at": item.created_at
                })).collect::<Vec<_>>()
            }))?,
            error: None,
        })
    }

    async fn execute_log(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = args
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter"))?;
        let since_seq = args.get("since_seq").and_then(|v| v.as_i64());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(256);
        let stream = args
            .get("stream")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if let Some(s) = stream {
            if s != "stdout" && s != "stderr" {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("stream must be 'stdout' or 'stderr' when provided".into()),
                });
            }
        }

        let exec_runtime = ShellExecRuntime::shared(self.security.clone(), self.runtime.clone())?;
        let Some(LogExecRunResult { items, next_seq }) = exec_runtime
            .log_run(run_id, since_seq, limit, stream)
            .await?
        else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Exec run not found: {run_id}")),
            });
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run_id,
                "since_seq": since_seq,
                "next_seq": next_seq,
                "items": items.iter().map(|item| json!({
                    "seq": item.seq,
                    "type": item.item_type,
                    "payload": item.payload,
                    "meta_json": item.meta_json,
                    "created_at": item.created_at
                })).collect::<Vec<_>>()
            }))?,
            error: None,
        })
    }

    async fn execute_kill(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let run_id = args
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'run_id' parameter"))?;

        let exec_runtime = ShellExecRuntime::shared(self.security.clone(), self.runtime.clone())?;
        let Some(run) = exec_runtime.stop_run(run_id).await? else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Exec run not found: {run_id}")),
            });
        };

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "run_id": run.run_id,
                "status": run.status,
                "finished_at": run.finished_at,
                "updated_at": run.updated_at
            }))?,
            error: None,
        })
    }
}

fn parse_watch_specs(raw: Option<&serde_json::Value>) -> anyhow::Result<Vec<ShellWatchSpec>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let watch_specs: Vec<ShellWatchSpec> = serde_json::from_value(raw.clone())?;
    if watch_specs.len() > MAX_WATCHERS {
        anyhow::bail!("watch exceeds max watcher count ({MAX_WATCHERS})");
    }

    for watch in &watch_specs {
        if watch.regex.len() > MAX_WATCH_REGEX_LEN {
            anyhow::bail!("watch regex exceeds max length ({MAX_WATCH_REGEX_LEN})");
        }
        if watch.regex.trim().is_empty() {
            anyhow::bail!("watch regex cannot be empty");
        }
        if watch.event.trim().is_empty() {
            anyhow::bail!("watch event cannot be empty");
        }
        if let Some(scope) = watch.scope.as_deref() {
            if !matches!(scope, "stdout" | "stderr") {
                anyhow::bail!("watch scope must be 'stdout' or 'stderr'");
            }
        }
        let _ = watch.once.unwrap_or(false);
        // Compile up-front so invalid patterns fail early.
        let _ = regex::Regex::new(watch.regex.as_str())?;
    }

    Ok(watch_specs)
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute shell commands (legacy sync mode or action-based async runs)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "poll", "log", "kill", "send_keys"],
                    "description": "Optional action selector. Omit for legacy synchronous shell execution"
                },
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk commands in supervised mode",
                    "default": false
                },
                "background": {
                    "type": "boolean",
                    "description": "When action=spawn, run in background (default true)",
                    "default": true
                },
                "pty": {
                    "type": "boolean",
                    "description": "When action=spawn, request PTY mode (phase 1 is non-PTY)",
                    "default": false
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Hard timeout for action=spawn"
                },
                "max_output_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Output cap for action=spawn"
                },
                "watch": {
                    "type": "array",
                    "description": "Watcher list: [{regex,event,once?,scope?}]",
                    "items": {
                        "type": "object",
                        "properties": {
                            "regex": {"type": "string"},
                            "event": {"type": "string"},
                            "once": {"type": "boolean"},
                            "scope": {"type": "string", "enum": ["stdout", "stderr"]}
                        },
                        "required": ["regex", "event"]
                    }
                },
                "session_id": {
                    "type": "string",
                    "description": "Creating session id for routing and backlog delivery"
                },
                "run_id": {
                    "type": "string",
                    "description": "Run identifier for action=poll|log|kill|send_keys"
                },
                "since_seq": {
                    "type": "integer",
                    "description": "Optional sequence cursor for incremental poll/log responses"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max items to return for action=log (server-capped)"
                },
                "stream": {
                    "type": "string",
                    "enum": ["stdout", "stderr"],
                    "description": "Optional stream filter for action=log"
                },
                "keys": {
                    "type": "string",
                    "description": "Optional key input for action=send_keys"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str());
        match action {
            Some("spawn") => self.execute_spawn(&args).await,
            Some("poll") => self.execute_poll(&args).await,
            Some("log") => self.execute_log(&args).await,
            Some("kill") => self.execute_kill(&args).await,
            Some("send_keys") => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("action=send_keys is not implemented in phase 1".to_string()),
            }),
            Some(other) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unsupported action: {other}")),
            }),
            None => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.execute_legacy(command, approved).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use crate::session::{backlog, ExecRunStatus, SessionStore};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use tokio::time::sleep;

    fn test_security(
        autonomy: AutonomyLevel,
        workspace_dir: &std::path::Path,
    ) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace_dir.to_path_buf(),
            ..SecurityPolicy::default()
        })
    }

    fn test_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(NativeRuntime::new())
    }

    async fn wait_for_terminal(tool: &ShellTool, run_id: &str) -> serde_json::Value {
        let start = Instant::now();
        loop {
            let result = tool
                .execute(json!({"action":"poll", "run_id": run_id}))
                .await
                .unwrap();
            let payload: serde_json::Value = serde_json::from_str(result.output.as_str()).unwrap();
            let status = payload["status"].as_str().unwrap_or_default();
            if matches!(status, "succeeded" | "failed" | "canceled" | "timed_out") {
                return payload;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn shell_tool_name() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );
        assert_eq!(tool.name(), "shell");
    }

    #[test]
    fn shell_tool_schema_has_action_and_command() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["command"].is_object());
    }

    #[tokio::test]
    async fn shell_legacy_call_without_action_behaves_as_before() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );
        let result = tool
            .execute(json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn shell_spawn_returns_run_id_and_persists_run() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let started = Instant::now();
        let result = tool
            .execute(json!({
                "action": "spawn",
                "command": "echo spawn-ok",
                "session_id": "session-spawn"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(started.elapsed() < Duration::from_secs(1));

        let payload: serde_json::Value = serde_json::from_str(result.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let store = SessionStore::new(tmp.path()).unwrap();
        let run = store.get_exec_run(run_id).unwrap().unwrap();
        assert_eq!(run.session_id, "session-spawn");
        assert_eq!(run.status, ExecRunStatus::Queued.as_str());
    }

    #[tokio::test]
    async fn shell_poll_returns_incremental_output() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let spawn = tool
            .execute(json!({
                "action": "spawn",
                "command": "echo a && echo b",
                "session_id": "session-poll"
            }))
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);

        let payload: serde_json::Value = serde_json::from_str(spawn.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let mut since_seq = 0_i64;
        let mut collected = String::new();
        let start = Instant::now();
        loop {
            let poll = tool
                .execute(json!({"action":"poll", "run_id": run_id, "since_seq": since_seq}))
                .await
                .unwrap();
            let poll_payload: serde_json::Value =
                serde_json::from_str(poll.output.as_str()).unwrap();

            for item in poll_payload["items"].as_array().unwrap() {
                if item["type"].as_str().unwrap_or_default() == "stdout" {
                    collected.push_str(item["payload"].as_str().unwrap_or_default());
                }
            }
            since_seq = poll_payload["next_seq"].as_i64().unwrap_or(since_seq);

            let status = poll_payload["status"].as_str().unwrap_or_default();
            if matches!(status, "succeeded" | "failed" | "canceled" | "timed_out") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            sleep(Duration::from_millis(30)).await;
        }

        assert!(collected.contains('a'));
        assert!(collected.contains('b'));
    }

    #[tokio::test]
    async fn shell_watcher_match_enqueues_backlog_item() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let spawn = tool
            .execute(json!({
                "action": "spawn",
                "command": "echo READY_SIGNAL",
                "session_id": "session-watch",
                "watch": [{"regex": "READY_SIGNAL", "event": "ready", "once": true}]
            }))
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let payload: serde_json::Value = serde_json::from_str(spawn.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let _ = wait_for_terminal(&tool, run_id).await;
        let backlog_items = backlog::drain("session-watch");
        assert!(backlog_items.iter().any(|item| item.contains("ready")));
    }

    #[tokio::test]
    async fn shell_kill_transitions_run_status() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let spawn = tool
            .execute(json!({
                "action": "spawn",
                "command": "tail -f Cargo.toml",
                "session_id": "session-kill"
            }))
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let payload: serde_json::Value = serde_json::from_str(spawn.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let killed = tool
            .execute(json!({"action":"kill", "run_id": run_id}))
            .await
            .unwrap();
        assert!(killed.success);

        let final_state = wait_for_terminal(&tool, run_id).await;
        assert_eq!(final_state["status"].as_str(), Some("canceled"));
    }

    #[test]
    fn test_security_with_env_cmd() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: tmp.path().to_path_buf(),
            allowed_commands: vec!["env".into(), "echo".into()],
            ..SecurityPolicy::default()
        });
        assert!(security.is_command_allowed("env"));
    }

    /// RAII guard that restores an environment variable to its original state on drop,
    /// ensuring cleanup even if the test panics.
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(val) => std::env::set_var(self.key, val),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    async fn shell_log_pagination_returns_items_and_next_seq() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let spawn = tool
            .execute(json!({
                "action": "spawn",
                "command": "echo one && echo two && echo three",
                "session_id": "session-log"
            }))
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let payload: serde_json::Value = serde_json::from_str(spawn.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let _ = wait_for_terminal(&tool, run_id).await;

        let log1 = tool
            .execute(json!({
                "action": "log",
                "run_id": run_id,
                "since_seq": 0,
                "limit": 2
            }))
            .await
            .unwrap();
        assert!(log1.success, "log failed: {:?}", log1.error);
        let out1: serde_json::Value = serde_json::from_str(log1.output.as_str()).unwrap();
        let items1 = out1["items"].as_array().unwrap();
        let next_seq1 = out1["next_seq"].as_i64().unwrap();
        assert!(!items1.is_empty(), "first log page should return items");
        assert!(next_seq1 >= 0);

        let log2 = tool
            .execute(json!({
                "action": "log",
                "run_id": run_id,
                "since_seq": next_seq1,
                "limit": 10
            }))
            .await
            .unwrap();
        assert!(log2.success);
        let out2: serde_json::Value = serde_json::from_str(log2.output.as_str()).unwrap();
        let items2 = out2["items"].as_array().unwrap();
        assert!(
            items2.is_empty() || out2["next_seq"].as_i64().unwrap() >= next_seq1,
            "second page cursor should advance or be empty"
        );
    }

    #[tokio::test]
    async fn shell_log_with_stream_filter_returns_only_that_stream() {
        let tmp = TempDir::new().unwrap();
        let tool = ShellTool::new(
            test_security(AutonomyLevel::Supervised, tmp.path()),
            test_runtime(),
        );

        let spawn = tool
            .execute(json!({
                "action": "spawn",
                "command": "echo one && echo two",
                "session_id": "session-log-stream"
            }))
            .await
            .unwrap();
        assert!(spawn.success, "spawn failed: {:?}", spawn.error);
        let payload: serde_json::Value = serde_json::from_str(spawn.output.as_str()).unwrap();
        let run_id = payload["run_id"].as_str().unwrap();

        let _ = wait_for_terminal(&tool, run_id).await;

        let log_stdout = tool
            .execute(json!({
                "action": "log",
                "run_id": run_id,
                "stream": "stdout",
                "limit": 50
            }))
            .await
            .unwrap();
        assert!(log_stdout.success);
        let out: serde_json::Value = serde_json::from_str(log_stdout.output.as_str()).unwrap();
        for item in out["items"].as_array().unwrap() {
            assert_eq!(item["type"].as_str(), Some("stdout"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shell_does_not_leak_api_key() {
        let tmp = TempDir::new().unwrap();
        let _g1 = EnvGuard::set("API_KEY", "sk-test-secret-12345");
        let _g2 = EnvGuard::set("ZEROCLAW_API_KEY", "sk-test-secret-67890");

        let tool = ShellTool::new(
            Arc::new(SecurityPolicy {
                autonomy: AutonomyLevel::Supervised,
                workspace_dir: tmp.path().to_path_buf(),
                allowed_commands: vec!["env".into()],
                ..SecurityPolicy::default()
            }),
            test_runtime(),
        );

        let result = tool.execute(json!({"command": "env"})).await.unwrap();
        assert!(result.success);
        assert!(!result.output.contains("sk-test-secret-12345"));
        assert!(!result.output.contains("sk-test-secret-67890"));
    }
}
