#[allow(unused_imports)]
use crate::observability::{Observer, ObserverEvent};
#[allow(unused_imports)]
use crate::providers::{ChatMessage, ChatRequest, ProviderCtx, ToolCall};
#[cfg(test)]
use crate::tools;
use crate::tools::Tool;
#[allow(unused_imports)]
use anyhow::Result;
use regex::{Regex, RegexSet};
#[allow(unused_imports)]
use std::sync::{Arc, LazyLock};
#[allow(unused_imports)]
use std::time::Instant;
use uuid::Uuid;

/// Maximum agentic tool-use iterations per user message to prevent runaway loops.
pub(crate) const MAX_TOOL_ITERATIONS: usize = 10;

/// Maximum characters for a single tool result before truncation (pseudo-code THRESHOLD).
const TOOL_OUTPUT_TRUNCATE_CHARS: usize = 16_384;

/// Plan constant for agent.rs: max chars before truncation with floor_char_boundary.
pub(crate) const TOOL_OUTPUT_MAX_CHARS: usize = 32_000;

/// Truncate tool output to TOOL_OUTPUT_MAX_CHARS (characters), appending "... N chars truncated".
pub(crate) fn maybe_truncate_tool_output(output: &str) -> String {
    let char_count = output.chars().count();
    if char_count <= TOOL_OUTPUT_MAX_CHARS {
        return output.to_string();
    }
    let boundary = output
        .char_indices()
        .take(TOOL_OUTPUT_MAX_CHARS)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let truncated = &output[..boundary];
    let remainder = char_count.saturating_sub(TOOL_OUTPUT_MAX_CHARS);
    format!("{}\n\n... {} chars truncated", truncated, remainder)
}

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
pub(crate) fn scrub_credentials(input: &str) -> String {
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
pub(crate) fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

pub(crate) fn bind_custom_tool_args(
    tool_name: &str,
    arguments: serde_json::Value,
    source_session_id: Option<&str>,
) -> serde_json::Value {
    let Some(source_session_id) = source_session_id else {
        return arguments;
    };

    let (field_name, inject) = match tool_name {
        "cron_add" | "cron_update" | "schedule" => ("source_session_id", true),
        "shell" => ("session_id", true),
        "subagent" => {
            let only_spawn = arguments
                .get("action")
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim() == "spawn")
                .unwrap_or(false);
            ("parent_session_id", only_spawn)
        }
        _ => return arguments,
    };
    if !inject {
        return arguments;
    }

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
pub(crate) fn build_assistant_content_native(text: &str, tool_calls: &[ToolCall]) -> String {
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
pub(crate) fn tool_results_to_chat_messages(results: &[ToolExecutionResult]) -> Vec<ChatMessage> {
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

/// One-turn agent loop for tests: builds history and runs an inline tool-call loop with injected provider context.
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
    let tool_specs = tools_to_specs(tools_registry, None);
    let tool_slice = if tool_specs.is_empty() {
        None
    } else {
        Some(tool_specs.as_slice())
    };

    for _iter in 0..MAX_TOOL_ITERATIONS {
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: "test".to_string(),
            model: provider_ctx.model.clone(),
            messages_count: history.len(),
        });

        let resp = provider_ctx
            .provider
            .chat(
                ChatRequest {
                    messages: history.as_slice(),
                    tools: tool_slice,
                },
                &provider_ctx.model,
                provider_ctx.temperature,
            )
            .await?;

        let text = resp.text_or_empty().to_string();
        let tool_calls = resp.tool_calls;

        if tool_calls.is_empty() {
            history.push(ChatMessage::assistant(text.clone()));
            return Ok((text, history));
        }

        let assistant_content = build_assistant_content_native(&text, &tool_calls);
        let mut execution_results: Vec<ToolExecutionResult> = Vec::with_capacity(tool_calls.len());
        for call in &tool_calls {
            let arguments_value = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            observer.record_event(&ObserverEvent::ToolCallStart {
                tool: call.name.clone(),
            });
            let start = Instant::now();
            let tool_args = bind_custom_tool_args(&call.name, arguments_value, None);
            let (output, success) = if let Some(tool) = find_tool(tools_registry, &call.name) {
                match tool.execute(tool_args).await {
                    Ok(r) => {
                        observer.record_event(&ObserverEvent::ToolCall {
                            tool: call.name.clone(),
                            duration: start.elapsed(),
                            success: r.success,
                        });
                        if r.success {
                            let scrubbed = scrub_credentials(&r.output);
                            let out = maybe_truncate_tool_output(&scrubbed);
                            (out, true)
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
        for m in tool_results_to_chat_messages(&execution_results) {
            history.push(m);
        }
    }

    anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
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
    fn bind_custom_tool_args_injects_parent_session_id_for_subagent_spawn() {
        let args = serde_json::json!({"action": "spawn"});
        let out = bind_custom_tool_args("subagent", args, Some("session-123"));
        assert_eq!(
            out.get("parent_session_id").and_then(|v| v.as_str()),
            Some("session-123")
        );
    }

    #[test]
    fn bind_custom_tool_args_injects_session_id_for_shell() {
        let args = serde_json::json!({"command": "ls"});
        let out = bind_custom_tool_args("shell", args, Some("session-shell"));
        assert_eq!(
            out.get("session_id").and_then(|v| v.as_str()),
            Some("session-shell")
        );
    }

    #[test]
    fn bind_custom_tool_args_does_not_override_existing() {
        let args = serde_json::json!({"action": "spawn", "parent_session_id": "existing"});
        let out = bind_custom_tool_args("subagent", args, Some("new-session"));
        assert_eq!(
            out.get("parent_session_id").and_then(|v| v.as_str()),
            Some("existing")
        );
    }

    #[test]
    fn bind_custom_tool_args_returns_unchanged_when_no_source_session_id() {
        let args = serde_json::json!({"action": "spawn"});
        let out = bind_custom_tool_args("subagent", args, None);
        assert!(out.get("parent_session_id").is_none());
    }

    #[test]
    fn bind_custom_tool_args_does_not_inject_for_subagent_send_or_stop() {
        let args_send = serde_json::json!({"action": "send", "session_id": "s1", "input": "hi"});
        let out_send = bind_custom_tool_args("subagent", args_send, Some("parent-1"));
        assert!(out_send.get("parent_session_id").is_none());

        let args_stop = serde_json::json!({"action": "stop", "session_id": "s1"});
        let out_stop = bind_custom_tool_args("subagent", args_stop, Some("parent-1"));
        assert!(out_stop.get("parent_session_id").is_none());
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
        const {
            assert!(MAX_TOOL_ITERATIONS > 0);
            assert!(MAX_TOOL_ITERATIONS <= 100);
        }
    };

    #[test]
    #[allow(clippy::assertions_on_constants)]
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
