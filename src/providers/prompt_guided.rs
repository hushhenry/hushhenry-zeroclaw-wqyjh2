//! Prompt-guided tool-use helpers for providers that do not support native tool calls.
//!
//! Build tool instructions for system/first-user injection, parse `<tool_call>` from model text,
//! and convert native-format history to plain (role, content) ChatMessages.

use crate::providers::traits::{ChatMessage, ToolCall};
use crate::tools::ToolSpec;
use serde_json::Value;
use std::fmt::Write;
use uuid::Uuid;

/// Build tool instructions string to inject into the conversation (e.g. first user message).
pub fn build_tool_instructions(specs: &[ToolSpec]) -> String {
    let mut instructions = String::new();
    instructions.push_str("## Tool Use Protocol\n\n");
    instructions.push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
    instructions.push_str(
        "<tool_call>\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n</tool_call>\n\n",
    );
    instructions.push_str("### Available Tools\n\n");
    for spec in specs {
        let parameters =
            serde_json::to_string(&spec.parameters).unwrap_or_else(|_| "{}".to_string());
        let _ = writeln!(
            instructions,
            "- **{}**: {}\n  Parameters: `{}`",
            spec.name, spec.description, parameters
        );
    }
    instructions
}

/// Parse `<tool_call>...</tool_call>` from model text; return (text_without_calls, tool_calls).
pub fn parse_tool_calls_from_text(response: &str) -> (String, Vec<ToolCall>) {
    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;

    while let Some(start) = remaining.find("<tool_call>") {
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }
        if let Some(end) = remaining[start..].find("</tool_call>") {
            let inner = &remaining[start + 11..start + end];
            if let Ok(parsed) = serde_json::from_str::<Value>(inner.trim()) {
                let name = parsed
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    let arguments = parsed
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                    let args_str =
                        serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string());
                    calls.push(ToolCall {
                        id: Uuid::new_v4().to_string(),
                        name,
                        arguments: args_str,
                    });
                }
            } else {
                tracing::warn!("Malformed <tool_call> JSON in prompt-guided response");
            }
            remaining = &remaining[start + end + 12..];
        } else {
            break;
        }
    }
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }
    (text_parts.join("\n"), calls)
}

/// Convert native-format history (assistant JSON + tool messages) to plain ChatMessages for prompt-guided APIs:
/// assistant content-only, tool results as one user message; inject instructions into first user.
pub fn convert_messages_to_prompt_guided(
    messages: &[ChatMessage],
    instructions: &str,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let m = &messages[i];
        if m.role == "assistant" {
            let content = if let Ok(value) = serde_json::from_str::<Value>(&m.content) {
                value
                    .get("content")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| m.content.clone())
            } else {
                m.content.clone()
            };
            out.push(ChatMessage {
                role: "assistant".to_string(),
                content,
            });
            i += 1;
        } else if m.role == "tool" {
            let mut tool_result_content = String::from("[Tool results]\n");
            while i < messages.len() && messages[i].role == "tool" {
                let t = &messages[i];
                if let Ok(value) = serde_json::from_str::<Value>(&t.content) {
                    let id = value
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let content = value
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or(&t.content);
                    let _ = writeln!(
                        tool_result_content,
                        "<tool_result id=\"{id}\">\n{content}\n</tool_result>"
                    );
                }
                i += 1;
            }
            out.push(ChatMessage {
                role: "user".to_string(),
                content: tool_result_content,
            });
            continue;
        } else if m.role == "user" {
            let is_first_user = !out.iter().any(|x| x.role == "user");
            let content = if is_first_user && !instructions.is_empty() {
                if m.content.is_empty() {
                    instructions.to_string()
                } else {
                    format!("{instructions}\n\n{}", m.content)
                }
            } else {
                m.content.clone()
            };
            out.push(ChatMessage {
                role: "user".to_string(),
                content,
            });
            i += 1;
        } else {
            out.push(ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            });
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tool_instructions_includes_protocol_and_tools() {
        let specs = vec![
            ToolSpec {
                name: "echo".to_string(),
                description: "Echoes the input".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            },
            ToolSpec {
                name: "shell".to_string(),
                description: "Run a shell command".to_string(),
                parameters: serde_json::json!({}),
            },
        ];
        let out = build_tool_instructions(&specs);
        assert!(out.contains("## Tool Use Protocol"));
        assert!(out.contains("<tool_call>"));
        assert!(out.contains("### Available Tools"));
        assert!(out.contains("**echo**"));
        assert!(out.contains("Echoes the input"));
        assert!(out.contains("**shell**"));
        assert!(out.contains("Run a shell command"));
    }

    #[test]
    fn build_tool_instructions_empty_specs() {
        let out = build_tool_instructions(&[]);
        assert!(out.contains("## Tool Use Protocol"));
        assert!(out.contains("### Available Tools"));
    }

    #[test]
    fn parse_tool_calls_from_text_extracts_single_call() {
        let response = r#"Let me check.
<tool_call>
{"name": "shell", "arguments": {"command": "ls -la"}}
</tool_call>"#;
        let (text, calls) = parse_tool_calls_from_text(response);
        assert_eq!(text, "Let me check.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args.get("command").unwrap().as_str().unwrap(), "ls -la");
        assert!(!calls[0].id.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_text_no_calls_returns_text() {
        let response = "Just a normal response.";
        let (text, calls) = parse_tool_calls_from_text(response);
        assert_eq!(text, "Just a normal response.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_text_multiple_calls() {
        let response = r#"<tool_call>
{"name": "a", "arguments": {}}
</tool_call>
<tool_call>
{"name": "b", "arguments": {"k": "v"}}
</tool_call>"#;
        let (text, calls) = parse_tool_calls_from_text(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
        let args1: serde_json::Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(args1.get("k").and_then(Value::as_str), Some("v"));
    }

    #[test]
    fn parse_tool_calls_from_text_skips_empty_name() {
        let response = r#"<tool_call>
{"name": "   ", "arguments": {}}
</tool_call>"#;
        let (_, calls) = parse_tool_calls_from_text(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_text_unclosed_tag_treats_as_text() {
        let response = "Before\n<tool_call>\n{\"name\": \"x\"}";
        let (text, calls) = parse_tool_calls_from_text(response);
        assert!(calls.is_empty());
        assert!(text.contains("Before"));
    }

    #[test]
    fn convert_messages_to_prompt_guided_injects_instructions_into_first_user() {
        let messages = vec![ChatMessage::user("hello"), ChatMessage::assistant("hi")];
        let out = convert_messages_to_prompt_guided(&messages, "## Instructions\nUse tools.");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        assert!(out[0].content.contains("## Instructions"));
        assert!(out[0].content.contains("hello"));
        assert_eq!(out[1].role, "assistant");
        assert_eq!(out[1].content, "hi");
    }

    #[test]
    fn convert_messages_to_prompt_guided_extracts_assistant_content_from_json() {
        let messages = vec![ChatMessage::assistant(
            r#"{"content": "I will run it.", "tool_calls": []}"#,
        )];
        let out = convert_messages_to_prompt_guided(&messages, "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "I will run it.");
    }

    #[test]
    fn convert_messages_to_prompt_guided_collapses_tool_results_into_one_user_message() {
        let messages = vec![
            ChatMessage::assistant("ok"),
            ChatMessage::tool(r#"{"tool_call_id": "tc1", "content": "result1"}"#),
            ChatMessage::tool(r#"{"tool_call_id": "tc2", "content": "result2"}"#),
        ];
        let out = convert_messages_to_prompt_guided(&messages, "");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[0].content, "ok");
        assert_eq!(out[1].role, "user");
        assert!(out[1].content.contains("[Tool results]"));
        assert!(out[1].content.contains("<tool_result"));
        assert!(out[1].content.contains("result1"));
        assert!(out[1].content.contains("result2"));
    }
}
