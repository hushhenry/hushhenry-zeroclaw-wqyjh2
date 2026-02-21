use crate::agent::dispatcher::{
    conversation_message_to_chat_messages, NativeToolDispatcher, ToolDispatcher,
    ToolExecutionResult, XmlToolDispatcher,
};
use crate::approval::{ApprovalManager, ApprovalRequest, ApprovalResponse};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, ChatRequest, ChatResponse, ProviderCtx, ToolCall};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use regex::{Regex, RegexSet};
use std::fmt::Write;
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

/// Inject dispatcher prompt instructions into a copy of messages (for prompt-guided tool use).
fn inject_prompt_instructions(
    messages: &[ChatMessage],
    instructions: &str,
) -> Vec<ChatMessage> {
    if instructions.is_empty() {
        return messages.to_vec();
    }
    let mut out = messages.to_vec();
    if let Some(system) = out.iter_mut().find(|m| m.role == "system") {
        if !system.content.is_empty() {
            system.content.push_str("\n\n");
        }
        system.content.push_str(instructions);
    } else {
        out.insert(0, ChatMessage::system(instructions.to_string()));
    }
    out
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
    approval: Option<&ApprovalManager>,
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

    let dispatcher: &dyn ToolDispatcher = if provider_ctx.provider.supports_native_tools() {
        &NativeToolDispatcher
    } else {
        &XmlToolDispatcher
    };

    for _iteration in 0..MAX_TOOL_ITERATIONS {
        let injected;
        let (messages_to_send, tools_to_send) = if dispatcher.should_send_tool_specs() {
            (history.as_slice(), tool_slice)
        } else {
            let instructions = dispatcher.prompt_instructions(&tool_specs);
            injected = inject_prompt_instructions(history, &instructions);
            (injected.as_slice(), None)
        };

        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: provider_ctx.model.clone(),
            messages_count: messages_to_send.len(),
        });

        let llm_started_at = Instant::now();

        let resp = match provider_ctx
            .provider
            .chat(
                ChatRequest {
                    messages: messages_to_send,
                    tools: tools_to_send,
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

        let result = dispatcher.parse_response(&resp);

        if result.tool_calls.is_empty() {
            history.push(ChatMessage::assistant(result.assistant_content_for_history));
            return Ok(result.text);
        }

        if !silent && !result.text.is_empty() {
            print!("{}", result.text);
            let _ = std::io::stdout().flush();
        }

        let mut execution_results: Vec<ToolExecutionResult> =
            Vec::with_capacity(result.tool_calls.len());
        for call in &result.tool_calls {
            if let Some(mgr) = approval {
                if mgr.needs_approval(&call.name) {
                    let request = ApprovalRequest {
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    };
                    let decision = if channel_name == "cli" {
                        mgr.prompt_cli(&request)
                    } else {
                        ApprovalResponse::Yes
                    };
                    mgr.record_decision(&call.name, &call.arguments, decision, channel_name);
                    if decision == ApprovalResponse::No {
                        execution_results.push(ToolExecutionResult {
                            name: call.name.clone(),
                            output: "Denied by user.".to_string(),
                            success: false,
                            tool_call_id: call.tool_call_id.clone(),
                        });
                        continue;
                    }
                }
            }

            observer.record_event(&ObserverEvent::ToolCallStart {
                tool: call.name.clone(),
            });
            let start = Instant::now();
            let tool_args = maybe_bind_source_session_id(
                &call.name,
                call.arguments.clone(),
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
                tool_call_id: call.tool_call_id.clone(),
            });
        }

        history.push(ChatMessage::assistant(result.assistant_content_for_history));
        for msg in conversation_message_to_chat_messages(dispatcher.format_results(&execution_results))
        {
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
        None,
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
    use tempfile::TempDir;

    fn parse_tool_calls_via_xml_dispatcher(
        response: &str,
    ) -> (String, Vec<crate::agent::dispatcher::ParsedToolCall>) {
        let resp = ChatResponse {
            text: Some(response.to_string()),
            tool_calls: vec![],
        };
        let result = XmlToolDispatcher.parse_response(&resp);
        (result.text, result.tool_calls)
    }

    #[test]
    fn parse_tool_calls_extracts_single_call() {
        let response = r#"Let me check that.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(text, "Let me check that.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_extracts_multiple_calls() {
        let response = r#"<tool_call>
{"name": "file_read", "arguments": {"path": "a.txt"}}
</tool_call>
<tool_call>
{"name": "file_read", "arguments": {"path": "b.txt"}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_returns_text_only_when_no_calls() {
        let response = "Just a normal response with no tools.";
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(text, "Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_malformed_json() {
        let response = r#"<tool_call>
not valid json
</tool_call>
Some text after."#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(calls.is_empty());
        assert!(text.contains("Some text after."));
    }

    #[test]
    fn parse_tool_calls_text_before_and_after() {
        let response = r#"Before text.
<tool_call>
{"name": "shell", "arguments": {"command": "echo hi"}}
</tool_call>
After text."#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(text.contains("Before text."));
        assert!(text.contains("After text."));
        assert_eq!(calls.len(), 1);
    }

    /// XmlToolDispatcher only parses <tool_call> tags; raw JSON is left as text (no tool_calls).
    #[test]
    fn parse_tool_calls_openai_style_json_left_as_text() {
        let response = r#"{"content": "Let me check.", "tool_calls": [{"type": "function", "function": {"name": "shell", "arguments": "{}"}}]}"#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(calls.is_empty(), "XmlToolDispatcher does not parse raw JSON");
        assert!(text.contains("Let me check"));
    }

    #[test]
    fn parse_tool_calls_rejects_raw_tool_json_without_tags() {
        // SECURITY: Raw JSON without explicit wrappers should NOT be parsed
        // This prevents prompt injection attacks where malicious content
        // could include JSON that mimics a tool call.
        let response = r#"Sure, creating the file now.
{"name": "file_write", "arguments": {"path": "hello.py", "content": "print('hello')"}}"#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(text.contains("Sure, creating the file now."));
        assert_eq!(
            calls.len(),
            0,
            "Raw JSON without wrappers should not be parsed"
        );
    }

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

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Parsing Edge Cases
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_calls_handles_empty_tool_result() {
        // Recovery: Empty tool_result tag should be handled gracefully
        let response = r#"I'll run that command.
<tool_result name="shell">

</tool_result>
Done."#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(text.contains("Done."));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_calls_array() {
        let response = r#"{"content": "Hello", "tool_calls": []}"#;
        let (text, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(text.contains("Hello"));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_whitespace_only_name() {
        let response = r#"<tool_call>
{"name": "   ", "arguments": {}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_empty_string_arguments() {
        let response = r#"<tool_call>
{"name": "test", "arguments": ""}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test");
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
    fn parse_tool_calls_via_dispatcher_handles_top_level_name() {
        let response = r#"<tool_call>
{"name": "test_tool", "arguments": {}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_tool");
    }

    #[test]
    fn parse_tool_calls_via_dispatcher_handles_empty_tool_calls_array() {
        let response = r#"{"tool_calls": []}"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_via_dispatcher_handles_missing_tool_calls() {
        let response = r#"<tool_call>
{"name": "test", "arguments": {}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test");
    }

    #[test]
    fn parse_tool_calls_via_dispatcher_handles_multiple_tags() {
        let response = r#"<tool_call>
{"name": "tool_a", "arguments": {}}
</tool_call>
<tool_call>
{"name": "tool_b", "arguments": {}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_via_xml_dispatcher(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "tool_a");
        assert_eq!(calls[1].name, "tool_b");
    }
}
