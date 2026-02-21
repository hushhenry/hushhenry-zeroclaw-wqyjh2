use crate::observability::{Observer, ObserverEvent};
use crate::providers::{ChatMessage, ChatRequest, ProviderCtx, ToolCall};
use crate::tools::{self, Tool};
use anyhow::Result;
use regex::{Regex, RegexSet};
use std::io::Write as _;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use uuid::Uuid;

/// Maximum agentic tool-use iterations per user message to prevent runaway loops.
const MAX_TOOL_ITERATIONS: usize = 10;

static SENSITIVE_KEY_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)token",
        r"(?i)api[_-]?key",
        r"(?i)password",
        r"(?i)secret",
        r"(?i)user[_-]?key",
        r"(?i)bearer",
        r"(?i)credential",
    ])
    .unwrap()
});

static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(token|api[_-]?key|password|secret|user[_-]?key|bearer|credential)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\.]{8,}))"#).unwrap()
});

/// Scrub credentials from tool output to prevent accidental exfiltration.
/// Replaces known credential patterns with a redacted placeholder while preserving
/// a small prefix for context.
fn scrub_credentials(input: &str) -> String {
    SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let full_match = &caps[0];
            let key = &caps[1];
            let val = caps
                .get(2)
                .or(caps.get(3))
                .or(caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");

            // Preserve first 4 chars for context, then redact
            let prefix = if val.len() > 4 { &val[..4] } else { "" };

            if full_match.contains(':') {
                if full_match.contains('"') {
                    format!("\"{}\": \"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}: {}*[REDACTED]", key, prefix)
                }
            } else if full_match.contains('=') {
                if full_match.contains('"') {
                    format!("{}=\"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}={}*[REDACTED]", key, prefix)
                }
            } else {
                format!("{}: {}*[REDACTED]", key, prefix)
            }
        })
        .to_string()
}

/// Convert a tool registry to provider-agnostic tool specifications.
/// When tool_allow_list is Some, only include tools whose name is in the list.
pub(crate) fn tools_to_specs(
    tools_registry: &[Box<dyn Tool>],
    tool_allow_list: Option<&[String]>,
) -> Vec<crate::tools::ToolSpec> {
    tools_registry
        .iter()
        .filter(|tool| {
            tool_allow_list
                .map(|allow| allow.iter().any(|n| n == tool.name()))
                .unwrap_or(true)
        })
        .map(|tool| tool.spec())
        .collect()
}

fn autosave_memory_key(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

/// Find a tool by name in the registry.
fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

fn maybe_bind_source_session_id(
    tool_name: &str,
    arguments: serde_json::Value,
    source_session_id: Option<&str>,
) -> serde_json::Value {
    let Some(source_session_id) = source_session_id else {
        return arguments;
    };

    let field_name = match tool_name {
        "cron_add" | "cron_update" | "schedule" => "source_session_id",
        "shell" => "session_id",
        "subagent_send" => "parent_session_id",
        _ => return arguments,
    };

    match arguments {
        serde_json::Value::Object(mut obj) => {
            obj.entry(field_name.to_string())
                .or_insert_with(|| serde_json::Value::String(source_session_id.to_string()));
            serde_json::Value::Object(obj)
        }
        _ => arguments,
    }
}

/// Result of executing one tool call; used to build tool-role messages for history.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub name: String,
    pub output: String,
    pub success: bool,
    pub tool_call_id: Option<String>,
}

/// Build the assistant message content (native JSON) to store in history after a tool round.
fn build_assistant_content_native(text: &str, tool_calls: &[ToolCall]) -> String {
    let tool_calls_value: Vec<serde_json::Value> = tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": serde_json::from_str::<serde_json::Value>(&tc.arguments).unwrap_or(serde_json::json!({})),
            })
        })
        .collect();
    serde_json::json!({
        "content": text,
        "tool_calls": tool_calls_value,
    })
    .to_string()
}

/// Convert tool execution results to tool-role ChatMessages for history.
fn tool_results_to_chat_messages(results: &[ToolExecutionResult]) -> Vec<ChatMessage> {
    results
        .iter()
        .map(|r| {
            ChatMessage::tool(
                serde_json::json!({
                    "tool_call_id": r.tool_call_id,
                    "content": r.output,
                })
                .to_string(),
            )
        })
        .collect()
}

/// Steer callback: at a tool-call checkpoint, called with the current user message content.
/// If the implementation has new messages (e.g. drained from a queue), it returns
/// Some(merged_content) to steer the next LLM call.
pub type SteerAtCheckpoint = dyn FnMut(&str) -> Option<String> + Send;

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
/// When tool_allow_list is Some, only those tools are exposed to the model and executable.
/// When steer_at_checkpoint is Some, it is used at safe boundaries.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_call_loop(
    provider_ctx: &ProviderCtx,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    tool_allow_list: Option<&[String]>,
    observer: &dyn Observer,
    provider_name: &str,
    silent: bool,
    channel_name: &str,
    source_session_id: Option<&str>,
    #[allow(clippy::type_complexity)] mut steer_at_checkpoint: Option<
        &mut (dyn FnMut(&str) -> Option<String> + Send + '_),
    >,
) -> Result<String> {
    let tool_specs = tools_to_specs(tools_registry, tool_allow_list);
    let tool_slice = if tool_specs.is_empty() {
        None
    } else {
        Some(tool_specs.as_slice())
    };

    for _iteration in 0..MAX_TOOL_ITERATIONS {
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: provider_ctx.model.clone(),
            messages_count: history.len(),
        });

        let llm_started_at = Instant::now();

        let resp = match provider_ctx
            .provider
            .chat(
                ChatRequest {
                    messages: history.as_slice(),
                    tools: tool_slice,
                },
                &provider_ctx.model,
                provider_ctx.temperature,
            )
            .await
        {
            Ok(r) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: provider_ctx.model.clone(),
                    duration: llm_started_at.elapsed(),
                    success: true,
                    error_message: None,
                });
                r
            }
            Err(e) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: provider_ctx.model.clone(),
                    duration: llm_started_at.elapsed(),
                    success: false,
                    error_message: Some(crate::providers::sanitize_api_error(&e.to_string())),
                });
                return Err(e);
            }
        };

        let text = resp.text_or_empty().to_string();
        let tool_calls = resp.tool_calls.clone();

        if tool_calls.is_empty() {
            history.push(ChatMessage::assistant(text.clone()));
            return Ok(text);
        }

        let assistant_content = build_assistant_content_native(&text, &tool_calls);

        if !silent && !text.is_empty() {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }

        let mut execution_results: Vec<ToolExecutionResult> = Vec::with_capacity(tool_calls.len());
        for call in &tool_calls {
            let arguments_value = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

            observer.record_event(&ObserverEvent::ToolCallStart {
                tool: call.name.clone(),
            });
            let start = Instant::now();
            let tool_args = maybe_bind_source_session_id(
                &call.name,
                arguments_value.clone(),
                source_session_id,
            );
            let disallowed =
                tool_allow_list.map(|allow| !allow.iter().any(|n| n.as_str() == call.name));
            let (output, success) = if disallowed == Some(true) {
                (
                    format!(
                        "Tool '{}' is not available for this agent (policy restriction).",
                        call.name
                    ),
                    false,
                )
            } else if let Some(tool) = find_tool(tools_registry, &call.name) {
                match tool.execute(tool_args).await {
                    Ok(r) => {
                        observer.record_event(&ObserverEvent::ToolCall {
                            tool: call.name.clone(),
                            duration: start.elapsed(),
                            success: r.success,
                        });
                        if r.success {
                            (scrub_credentials(&r.output), true)
                        } else {
                            (
                                format!("Error: {}", r.error.unwrap_or_else(|| r.output)),
                                false,
                            )
                        }
                    }
                    Err(e) => {
                        observer.record_event(&ObserverEvent::ToolCall {
                            tool: call.name.clone(),
                            duration: start.elapsed(),
                            success: false,
                        });
                        (format!("Error executing {}: {e}", call.name), false)
                    }
                }
            } else {
                (format!("Unknown tool: {}", call.name), false)
            };

            execution_results.push(ToolExecutionResult {
                name: call.name.clone(),
                output,
                success,
                tool_call_id: Some(call.id.clone()),
            });
        }

        history.push(ChatMessage::assistant(assistant_content));
        for msg in tool_results_to_chat_messages(&execution_results) {
            history.push(msg);
        }

        // Safe boundary (after each tool round): check agent queue and steer to merged content.
        let current_user_content = history
            .iter()
            .rev()
            .find(|m| m.role == "user" && !m.content.starts_with("[Tool results]"))
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let merged = steer_at_checkpoint
            .as_mut()
            .and_then(|steer| steer(current_user_content));
        if let Some(merged_content) = merged {
            history.push(ChatMessage::user(merged_content));
        }
    }

    anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
}

/// One-turn agent loop for tests: builds history and runs run_tool_call_loop with injected provider context.
/// Returns (response_text, final_history) so tests can assert on history when needed.
#[cfg(test)]
pub(crate) async fn run_one_turn_for_test(
    provider_ctx: &ProviderCtx,
    system_prompt: &str,
    user_message: &str,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
) -> Result<(String, Vec<ChatMessage>)> {
    let mut history = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_message),
    ];
    let response = run_tool_call_loop(
        provider_ctx,
        &mut history,
        tools_registry,
        None,
        observer,
        "test",
        true,
        "test",
        None,
        None,
    )
    .await?;
    Ok((response, history))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrub_credentials() {
        let input = "API_KEY=sk-1234567890abcdef; token: 1234567890; password=\"secret123456\"";
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("API_KEY=sk-1*[REDACTED]"));
        assert!(scrubbed.contains("token: 1234*[REDACTED]"));
        assert!(scrubbed.contains("password=\"secr*[REDACTED]\""));
        assert!(!scrubbed.contains("abcdef"));
        assert!(!scrubbed.contains("secret123456"));
    }

    #[test]
    fn test_scrub_credentials_json() {
        let input = r#"{"api_key": "sk-1234567890", "other": "public"}"#;
        let scrubbed = scrub_credentials(input);
        assert!(scrubbed.contains("\"api_key\": \"sk-1*[REDACTED]\""));
        assert!(scrubbed.contains("public"));
    }

    #[test]
    fn maybe_bind_source_session_id_injects_parent_session_id_for_subagent_send() {
        let args = serde_json::json!({"prompt": "task"});
        let out = maybe_bind_source_session_id("subagent_send", args, Some("session-123"));
        assert_eq!(
            out.get("parent_session_id").and_then(|v| v.as_str()),
            Some("session-123")
        );
    }

    #[test]
    fn maybe_bind_source_session_id_injects_session_id_for_shell() {
        let args = serde_json::json!({"command": "ls"});
        let out = maybe_bind_source_session_id("shell", args, Some("session-shell"));
        assert_eq!(
            out.get("session_id").and_then(|v| v.as_str()),
            Some("session-shell")
        );
    }

    #[test]
    fn maybe_bind_source_session_id_does_not_override_existing() {
        let args = serde_json::json!({"prompt": "task", "parent_session_id": "existing"});
        let out = maybe_bind_source_session_id("subagent_send", args, Some("new-session"));
        assert_eq!(
            out.get("parent_session_id").and_then(|v| v.as_str()),
            Some("existing")
        );
    }

    #[test]
    fn maybe_bind_source_session_id_returns_unchanged_when_no_source_session_id() {
        let args = serde_json::json!({"prompt": "task"});
        let out = maybe_bind_source_session_id("subagent_send", args, None);
        assert!(out.get("parent_session_id").is_none());
    }

    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use crate::providers::prompt_guided;
    use tempfile::TempDir;

    #[test]
    fn tools_to_specs_respects_allow_list() {
        use crate::security::SecurityPolicy;
        let security = Arc::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        ));
        let tools = tools::default_tools(security);
        let allow = vec!["shell".to_string(), "file_read".to_string()];
        let specs = tools_to_specs(&tools, Some(&allow));
        let names: Vec<&str> = specs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
    }

    #[test]
    fn tools_to_specs_produces_valid_schema() {
        use crate::security::SecurityPolicy;
        let security = Arc::new(SecurityPolicy::from_config(
            &crate::config::AutonomyConfig::default(),
            std::path::Path::new("/tmp"),
        ));
        let tools = tools::default_tools(security);
        let formatted = tools_to_specs(&tools, None);

        assert!(!formatted.is_empty());
        for tool in &formatted {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            assert!(tool.parameters.is_object());
        }
        // Verify known tools are present
        let names: Vec<&str> = formatted.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
    }

    #[test]
    fn autosave_memory_key_has_prefix_and_uniqueness() {
        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        assert!(key1.starts_with("user_msg_"));
        assert!(key2.starts_with("user_msg_"));
        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn autosave_memory_keys_preserve_multiple_turns() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();

        let key1 = autosave_memory_key("user_msg");
        let key2 = autosave_memory_key("user_msg");

        mem.store(&key1, "I'm Paul", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        mem.store(&key2, "I'm 45", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);

        let recalled = mem.recall("45", 5, None).await.unwrap();
        assert!(recalled.iter().any(|entry| entry.content.contains("45")));
    }

    const _: () = {
        assert!(MAX_TOOL_ITERATIONS > 0);
        assert!(MAX_TOOL_ITERATIONS <= 100);
    };

    #[test]
    fn constants_bounds_are_compile_time_checked() {
        assert!(MAX_TOOL_ITERATIONS > 0);
        assert!(MAX_TOOL_ITERATIONS <= 100);
    }

    #[test]
    fn ollama_prompt_guided_parse_extracts_single_call() {
        let response = r#"Let me check.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;
        let (text, calls) = prompt_guided::parse_tool_calls_from_text(response);
        assert_eq!(text, "Let me check.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "ls -la");
    }

    #[test]
    fn ollama_prompt_guided_parse_returns_text_only_when_no_calls() {
        let response = "Just a normal response.";
        let (text, calls) = prompt_guided::parse_tool_calls_from_text(response);
        assert_eq!(text, "Just a normal response.");
        assert!(calls.is_empty());
    }

    #[test]
    fn ollama_prompt_guided_parse_raw_json_left_as_text() {
        let response = r#"{"content": "Hi.", "tool_calls": []}"#;
        let (text, calls) = prompt_guided::parse_tool_calls_from_text(response);
        assert!(calls.is_empty());
        assert!(text.contains("Hi."));
    }
}
