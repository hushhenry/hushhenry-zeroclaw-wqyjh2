use crate::approval::{ApprovalManager, ApprovalRequest, ApprovalResponse};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, ChatRequest, Provider, ToolCall};
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

fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        Some(value) => value.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    if let Some(function) = value.get("function") {
        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !name.is_empty() {
            let arguments = parse_arguments_value(function.get("arguments"));
            return Some(ParsedToolCall { name, arguments });
        }
    }

    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if name.is_empty() {
        return None;
    }

    let arguments = parse_arguments_value(value.get("arguments"));
    Some(ParsedToolCall { name, arguments })
}

fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    if let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(parsed) = parse_tool_call_value(call) {
                calls.push(parsed);
            }
        }

        if !calls.is_empty() {
            return calls;
        }
    }

    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = parse_tool_call_value(item) {
                calls.push(parsed);
            }
        }
        return calls;
    }

    if let Some(parsed) = parse_tool_call_value(value) {
        calls.push(parsed);
    }

    calls
}

const TOOL_CALL_OPEN_TAGS: [&str; 3] = ["<tool_call>", "<toolcall>", "<tool-call>"];

fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
        .min_by_key(|(idx, _)| *idx)
}

fn matching_tool_call_close_tag(open_tag: &str) -> Option<&'static str> {
    match open_tag {
        "<tool_call>" => Some("</tool_call>"),
        "<toolcall>" => Some("</toolcall>"),
        "<tool-call>" => Some("</tool-call>"),
        _ => None,
    }
}

/// Extract JSON values from a string.
///
/// # Security Warning
///
/// This function extracts ANY JSON objects/arrays from the input. It MUST only
/// be used on content that is already trusted to be from the LLM, such as
/// content inside `<invoke>` tags where the LLM has explicitly indicated intent
/// to make a tool call. Do NOT use this on raw user input or content that
/// could contain prompt injection payloads.
fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return values;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        values.push(value);
        return values;
    }

    let char_positions: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut idx = 0;
    while idx < char_positions.len() {
        let (byte_idx, ch) = char_positions[idx];
        if ch == '{' || ch == '[' {
            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    values.push(value);
                    let next_byte = byte_idx + consumed;
                    while idx < char_positions.len() && char_positions[idx].0 < next_byte {
                        idx += 1;
                    }
                    continue;
                }
            }
        }
        idx += 1;
    }

    values
}

/// Parse tool calls from an LLM response that uses XML-style function calling.
///
/// Expected format (common with system-prompt-guided tool use):
/// ```text
/// <tool_call>
/// {"name": "shell", "arguments": {"command": "ls"}}
/// </tool_call>
/// ```
///
/// Also accepts common tag variants (`<toolcall>`, `<tool-call>`) for model
/// compatibility.
///
/// Also supports JSON with `tool_calls` array from OpenAI-format responses.
fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;

    // First, try to parse as OpenAI-style JSON response with tool_calls array
    // This handles providers like Minimax that return tool_calls in native JSON format
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        calls = parse_tool_calls_from_json_value(&json_value);
        if !calls.is_empty() {
            // If we found tool_calls, extract any content field as text
            if let Some(content) = json_value.get("content").and_then(|v| v.as_str()) {
                if !content.trim().is_empty() {
                    text_parts.push(content.trim().to_string());
                }
            }
            return (text_parts.join("\n"), calls);
        }
    }

    // Fall back to XML-style tool-call tag parsing.
    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        // Everything before the tag is text
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        let Some(close_tag) = matching_tool_call_close_tag(open_tag) else {
            break;
        };

        let after_open = &remaining[start + open_tag.len()..];
        if let Some(close_idx) = after_open.find(close_tag) {
            let inner = &after_open[..close_idx];
            let mut parsed_any = false;
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    parsed_any = true;
                    calls.extend(parsed_calls);
                }
            }

            if !parsed_any {
                tracing::warn!("Malformed <tool_call> JSON: expected tool-call object in tag body");
            }

            remaining = &after_open[close_idx + close_tag.len()..];
        } else {
            break;
        }
    }

    // SECURITY: We do NOT fall back to extracting arbitrary JSON from the response
    // here. That would enable prompt injection attacks where malicious content
    // (e.g., in emails, files, or web pages) could include JSON that mimics a
    // tool call. Tool calls MUST be explicitly wrapped in either:
    // 1. OpenAI-style JSON with a "tool_calls" array
    // 2. ZeroClaw tool-call tags (<tool_call>, <toolcall>, <tool-call>)
    // This ensures only the LLM's intentional tool calls are executed.

    // Remaining text after last tool call
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

fn parse_structured_tool_calls(tool_calls: &[ToolCall]) -> Vec<ParsedToolCall> {
    tool_calls
        .iter()
        .map(|call| ParsedToolCall {
            name: call.name.clone(),
            arguments: serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        })
        .collect()
}

fn build_assistant_history_with_tool_calls(text: &str, tool_calls: &[ToolCall]) -> String {
    let mut parts = Vec::new();

    if !text.trim().is_empty() {
        parts.push(text.trim().to_string());
    }

    for call in tool_calls {
        let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        let payload = serde_json::json!({
            "id": call.id,
            "name": call.name,
            "arguments": arguments,
        });
        parts.push(format!("<tool_call>\n{payload}\n</tool_call>"));
    }

    parts.join("\n")
}

#[derive(Debug)]
struct ParsedToolCall {
    name: String,
    arguments: serde_json::Value,
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
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    tool_allow_list: Option<&[String]>,
    observer: &dyn Observer,
    provider_name: &str,
    model: &str,
    temperature: f64,
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

    for _iteration in 0..MAX_TOOL_ITERATIONS {
        observer.record_event(&ObserverEvent::LlmRequest {
            provider: provider_name.to_string(),
            model: model.to_string(),
            messages_count: history.len(),
        });

        let llm_started_at = Instant::now();

        let (response_text, parsed_text, tool_calls, assistant_history_content) = match provider
            .chat(
                ChatRequest {
                    messages: history,
                    tools: tool_slice,
                },
                model,
                temperature,
            )
            .await
        {
            Ok(resp) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration: llm_started_at.elapsed(),
                    success: true,
                    error_message: None,
                });
                let response_text = resp.text_or_empty().to_string();
                let mut calls = parse_structured_tool_calls(&resp.tool_calls);
                let mut parsed_text = String::new();

                if calls.is_empty() {
                    let (fallback_text, fallback_calls) = parse_tool_calls(&response_text);
                    if !fallback_text.is_empty() {
                        parsed_text = fallback_text;
                    }
                    calls = fallback_calls;
                }

                let assistant_history_content = if resp.tool_calls.is_empty() {
                    response_text.clone()
                } else {
                    build_assistant_history_with_tool_calls(&response_text, &resp.tool_calls)
                };

                (response_text, parsed_text, calls, assistant_history_content)
            }
            Err(e) => {
                observer.record_event(&ObserverEvent::LlmResponse {
                    provider: provider_name.to_string(),
                    model: model.to_string(),
                    duration: llm_started_at.elapsed(),
                    success: false,
                    error_message: Some(crate::providers::sanitize_api_error(&e.to_string())),
                });
                return Err(e);
            }
        };

        let display_text = if parsed_text.is_empty() {
            response_text.clone()
        } else {
            parsed_text
        };

        if tool_calls.is_empty() {
            // No tool calls — this is the final response
            history.push(ChatMessage::assistant(response_text.clone()));
            return Ok(display_text);
        }

        // Print any text the LLM produced alongside tool calls (unless silent)
        if !silent && !display_text.is_empty() {
            print!("{display_text}");
            let _ = std::io::stdout().flush();
        }

        // Execute each tool call and build results
        let mut tool_results = String::new();
        for call in &tool_calls {
            // ── Approval hook ────────────────────────────────
            if let Some(mgr) = approval {
                if mgr.needs_approval(&call.name) {
                    let request = ApprovalRequest {
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    };

                    // Only prompt interactively on CLI; auto-approve on other channels.
                    let decision = if channel_name == "cli" {
                        mgr.prompt_cli(&request)
                    } else {
                        ApprovalResponse::Yes
                    };

                    mgr.record_decision(&call.name, &call.arguments, decision, channel_name);

                    if decision == ApprovalResponse::No {
                        let _ = writeln!(
                            tool_results,
                            "<tool_result name=\"{}\">\nDenied by user.\n</tool_result>",
                            call.name
                        );
                        continue;
                    }
                }
            }

            observer.record_event(&ObserverEvent::ToolCallStart {
                tool: call.name.clone(),
            });
            let start = Instant::now();
            let tool_args =
                maybe_bind_source_session_id(&call.name, call.arguments.clone(), source_session_id);
            let disallowed =
                tool_allow_list.map(|allow| !allow.iter().any(|n| n.as_str() == call.name));
            let result = if disallowed == Some(true) {
                format!(
                    "Tool '{}' is not available for this agent (policy restriction).",
                    call.name
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
                            scrub_credentials(&r.output)
                        } else {
                            format!("Error: {}", r.error.unwrap_or_else(|| r.output))
                        }
                    }
                    Err(e) => {
                        observer.record_event(&ObserverEvent::ToolCall {
                            tool: call.name.clone(),
                            duration: start.elapsed(),
                            success: false,
                        });
                        format!("Error executing {}: {e}", call.name)
                    }
                }
            } else {
                format!("Unknown tool: {}", call.name)
            };

            let _ = writeln!(
                tool_results,
                "<tool_result name=\"{}\">\n{}\n</tool_result>",
                call.name, result
            );
        }

        // Add assistant message with tool calls + tool results to history
        history.push(ChatMessage::assistant(assistant_history_content));
        history.push(ChatMessage::user(format!("[Tool results]\n{tool_results}")));

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

/// One-turn agent loop for tests: builds history and runs run_tool_call_loop with injected provider.
/// Returns (response_text, final_history) so tests can assert on history when needed.
#[cfg(test)]
pub(crate) async fn run_one_turn_for_test(
    provider: &dyn Provider,
    system_prompt: &str,
    user_message: &str,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    model: &str,
    temperature: f64,
) -> Result<(String, Vec<ChatMessage>)> {
    let mut history = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_message),
    ];
    let response = run_tool_call_loop(
        provider,
        &mut history,
        tools_registry,
        None,
        observer,
        "test",
        model,
        temperature,
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

    #[test]
    fn parse_tool_calls_extracts_single_call() {
        let response = r#"Let me check that.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
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

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_returns_text_only_when_no_calls() {
        let response = "Just a normal response with no tools.";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_malformed_json() {
        let response = r#"<tool_call>
not valid json
</tool_call>
Some text after."#;

        let (text, calls) = parse_tool_calls(response);
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

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Before text."));
        assert!(text.contains("After text."));
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_handles_openai_format() {
        // OpenAI-style response with tool_calls array
        let response = r#"{"content": "Let me check that for you.", "tool_calls": [{"type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"ls -la\"}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that for you.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_handles_openai_format_multiple_calls() {
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"a.txt\"}"}}, {"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"b.txt\"}"}}]}"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_openai_format_without_content() {
        // Some providers don't include content field with tool_calls
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "memory_recall", "arguments": "{}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty()); // No content field
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
    }

    #[test]
    fn parse_tool_calls_handles_markdown_json_inside_tool_call_tag() {
        let response = r#"<tool_call>
```json
{"name": "file_write", "arguments": {"path": "test.py", "content": "print('ok')"}}
```
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "test.py"
        );
    }

    #[test]
    fn parse_tool_calls_handles_noisy_tool_call_tag_body() {
        let response = r#"<tool_call>
I will now call the tool with this payload:
{"name": "shell", "arguments": {"command": "pwd"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_toolcall_tag_alias() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</toolcall>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_dash_call_tag_alias() {
        let response = r#"<tool-call>
{"name": "shell", "arguments": {"command": "whoami"}}
</tool-call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "whoami"
        );
    }

    #[test]
    fn parse_tool_calls_does_not_cross_match_alias_tags() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
        assert!(text.contains("<toolcall>"));
        assert!(text.contains("</tool_call>"));
    }

    #[test]
    fn parse_tool_calls_rejects_raw_tool_json_without_tags() {
        // SECURITY: Raw JSON without explicit wrappers should NOT be parsed
        // This prevents prompt injection attacks where malicious content
        // could include JSON that mimics a tool call.
        let response = r#"Sure, creating the file now.
{"name": "file_write", "arguments": {"path": "hello.py", "content": "print('hello')"}}"#;

        let (text, calls) = parse_tool_calls(response);
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
        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Done."));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_null() {
        // Recovery: null arguments are returned as-is (Value::Null)
        let value = serde_json::json!(null);
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_null());
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_calls_array() {
        // Recovery: Empty tool_calls array returns original response (no tool parsing)
        let response = r#"{"content": "Hello", "tool_calls": []}"#;
        let (text, calls) = parse_tool_calls(response);
        // When tool_calls is empty, the entire JSON is returned as text
        assert!(text.contains("Hello"));
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_whitespace_only_name() {
        // Recovery: Whitespace-only tool name should return None
        let value = serde_json::json!({"function": {"name": "   ", "arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_calls_handles_empty_string_arguments() {
        // Recovery: Empty string arguments should be handled
        let value = serde_json::json!({"name": "test", "arguments": ""});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Arguments Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_arguments_value_handles_invalid_json_string() {
        // Recovery: Invalid JSON string should return empty object
        let value = serde_json::Value::String("not valid json".to_string());
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_none() {
        // Recovery: None arguments should return empty object
        let result = parse_arguments_value(None);
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - JSON Extraction
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn extract_json_values_handles_empty_string() {
        // Recovery: Empty input should return empty vec
        let result = extract_json_values("");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_whitespace_only() {
        // Recovery: Whitespace only should return empty vec
        let result = extract_json_values("   \n\t  ");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_multiple_objects() {
        // Recovery: Multiple JSON objects should all be extracted
        let input = r#"{"a": 1}{"b": 2}{"c": 3}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn extract_json_values_handles_arrays() {
        // Recovery: JSON arrays should be extracted
        let input = r#"[1, 2, 3]{"key": "value"}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 2);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Constants Validation
    // ═══════════════════════════════════════════════════════════════════════

    const _: () = {
        assert!(MAX_TOOL_ITERATIONS > 0);
        assert!(MAX_TOOL_ITERATIONS <= 100);
    };

    #[test]
    fn constants_bounds_are_compile_time_checked() {
        // Bounds are enforced by the const assertions above.
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Recovery Tests - Tool Call Value Parsing
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_call_value_handles_missing_name_field() {
        // Recovery: Missing name field should return None
        let value = serde_json::json!({"function": {"arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_call_value_handles_top_level_name() {
        // Recovery: Tool call with name at top level (non-OpenAI format)
        let value = serde_json::json!({"name": "test_tool", "arguments": {}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test_tool");
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_empty_array() {
        // Recovery: Empty tool_calls array should return empty vec
        let value = serde_json::json!({"tool_calls": []});
        let result = parse_tool_calls_from_json_value(&value);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_missing_tool_calls() {
        // Recovery: Missing tool_calls field should fall through
        let value = serde_json::json!({"name": "test", "arguments": {}});
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_top_level_array() {
        // Recovery: Top-level array of tool calls
        let value = serde_json::json!([
            {"name": "tool_a", "arguments": {}},
            {"name": "tool_b", "arguments": {}}
        ]);
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 2);
    }
}
