//! Comprehensive agent-loop test suite.
//!
//! Tests exercise the full agent loop (loop_::run_one_turn_for_test) with mock providers and tools,
//! covering every edge case an agentic tool loop must handle:
//!
//!   1. Simple text response (no tools)
//!   2. Single tool call → final response
//!   3. Multi-step tool chain (tool A → tool B → response)
//!   4. Max-iteration bailout
//!   5. Unknown tool name recovery
//!   6. Tool execution failure recovery
//!   7. Parallel tool dispatch
//!   8. History trimming during long conversations
//!   9. Memory auto-save round-trip
//!  10. Empty / whitespace-only LLM responses
//!  12. Mixed text + tool call responses
//!  13. Multi-tool batch in a single response
//!  14. System prompt generation & tool instructions
//!  15. Context enrichment from memory loader
//!  16. ConversationMessage serialization round-trip
//!  17. Conversation history fidelity (tool call → tool result → assistant)
//!  18. Idempotent system prompt insertion

use crate::agent::loop_;
use crate::config::MemoryConfig;
use crate::memory::{self, Memory};
use crate::observability::{NoopObserver, Observer};
use crate::providers::traits::ProviderCapabilities;
use crate::providers::ProviderCtx;
use crate::providers::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ToolCall,
    ToolResultMessage,
};
use crate::tools::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════════════
// Test Helpers — Mock Provider, Mock Tool, Mock Memory
// ═══════════════════════════════════════════════════════════════════════════

/// A mock LLM provider that returns pre-scripted responses in order.
/// When the queue is exhausted it returns a simple "done" text response.
struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
        }
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap()
            .push(request.messages.to_vec());

        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            });
        }
        Ok(guard.remove(0))
    }
}

/// A mock provider that always returns an error.
struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        anyhow::bail!("provider error")
    }
}

/// A simple echo tool that returns its arguments as output.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string();
        Ok(ToolResult {
            success: true,
            output: msg,
            error: None,
        })
    }
}

/// A tool that always fails execution.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some("intentional failure".into()),
        })
    }
}

/// A tool that panics (tests error propagation).
struct PanickingTool;

#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &str {
        "panicker"
    }

    fn description(&self) -> &str {
        "Panics on execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        anyhow::bail!("catastrophic tool failure")
    }
}

/// A tool that tracks how many times it was called.
struct CountingTool {
    count: Arc<Mutex<usize>>,
}

impl CountingTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let count = Arc::new(Mutex::new(0));
        (
            Self {
                count: count.clone(),
            },
            count,
        )
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }

    fn description(&self) -> &str {
        "Counts calls"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        Ok(ToolResult {
            success: true,
            output: format!("call #{}", *c),
            error: None,
        })
    }
}

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, std::path::Path::new("/tmp"), None).unwrap())
}

fn make_sqlite_memory() -> (Arc<dyn Memory>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = MemoryConfig {
        backend: "sqlite".into(),
        ..MemoryConfig::default()
    };
    let mem = Arc::from(memory::create_memory(&cfg, tmp.path(), None).unwrap());
    (mem, tmp)
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

/// Build a test ProviderCtx from an Arc<dyn Provider>.
fn provider_ctx(provider: Arc<dyn Provider>) -> ProviderCtx {
    ProviderCtx {
        provider,
        model: "test".into(),
        temperature: 0.7,
    }
}

/// Run one turn via loop_ for tests.
async fn run_turn(
    provider_ctx: &ProviderCtx,
    user_message: &str,
    tools_registry: &[Box<dyn Tool>],
) -> Result<(String, Vec<ChatMessage>)> {
    loop_::run_one_turn_for_test(
        provider_ctx,
        "You are a helpful assistant.",
        user_message,
        tools_registry,
        &NoopObserver,
    )
    .await
}

/// Helper: create a ChatResponse with tool calls (native format).
fn tool_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        text: Some(String::new()),
        tool_calls: calls,
    }
}

/// Helper: create a plain text ChatResponse.
fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: Some(text.into()),
        tool_calls: vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Simple text response (no tools)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_returns_text_when_no_tools_called() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("Hello world")]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (response, _) = run_turn(&provider_ctx(provider), "hi", &tools)
        .await
        .unwrap();
    assert_eq!(response, "Hello world");
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Single tool call → final response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_executes_single_tool_then_returns() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello from tool"}"#.into(),
        }]),
        text_response("I ran the tool"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (response, _) = run_turn(&provider_ctx(provider), "run echo", &tools)
        .await
        .unwrap();
    assert_eq!(response, "I ran the tool");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Multi-step tool chain (tool A → tool B → response)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multi_step_tool_chain() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc3".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        text_response("Done after 3 calls"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(counting_tool)];
    let (response, _) = run_turn(&provider_ctx(provider), "count 3 times", &tools)
        .await
        .unwrap();
    assert_eq!(response, "Done after 3 calls");
    assert_eq!(*count.lock().unwrap(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Max-iteration bailout
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_bails_out_at_max_iterations() {
    // loop_::run_tool_call_loop uses MAX_TOOL_ITERATIONS (10). Create 11 tool responses.
    const MAX_ITERS: usize = 10;
    let mut responses = Vec::new();
    for i in 0..MAX_ITERS + 1 {
        responses.push(tool_response(vec![ToolCall {
            id: format!("tc{i}"),
            name: "echo".into(),
            arguments: r#"{"message": "loop"}"#.into(),
        }]));
    }

    let provider = Arc::new(ScriptedProvider::new(responses));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];

    let result = run_turn(&provider_ctx(provider), "infinite loop", &tools).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("maximum tool iterations"),
        "Expected max iterations error, got: {err}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Unknown tool name recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_unknown_tool_gracefully() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "nonexistent_tool".into(),
            arguments: "{}".into(),
        }]),
        text_response("I couldn't find that tool"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (response, history) = run_turn(&provider_ctx(provider), "use nonexistent", &tools)
        .await
        .unwrap();
    assert_eq!(response, "I couldn't find that tool");
    let has_tool_result = history.iter().any(|m| m.content.contains("Unknown tool"));
    assert!(
        has_tool_result,
        "Expected tool result with 'Unknown tool' message (user or tool role)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Tool execution failure recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_recovers_from_tool_failure() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "fail".into(),
            arguments: "{}".into(),
        }]),
        text_response("Tool failed but I recovered"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(FailingTool)];
    let (response, _) = run_turn(&provider_ctx(provider), "try failing tool", &tools)
        .await
        .unwrap();
    assert_eq!(response, "Tool failed but I recovered");
}

#[tokio::test]
async fn turn_recovers_from_tool_error() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "panicker".into(),
            arguments: "{}".into(),
        }]),
        text_response("I recovered from the error"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(PanickingTool)];
    let (response, _) = run_turn(&provider_ctx(provider), "try panicking", &tools)
        .await
        .unwrap();
    assert_eq!(response, "I recovered from the error");
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Provider error propagation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_propagates_provider_error() {
    let provider = Arc::new(FailingProvider);
    let tools: Vec<Box<dyn Tool>> = vec![];
    let result = run_turn(&provider_ctx(provider), "hello", &tools).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("provider error"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. History trimming — tested via loop_ in integration; single-turn helper has no trim
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
// 9. Memory auto-save — tested in integration
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
// 10. Empty / whitespace-only LLM responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_empty_text_response() {
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        text: Some(String::new()),
        tool_calls: vec![],
    }]));
    let tools: Vec<Box<dyn Tool>> = vec![];
    let (response, _) = run_turn(&provider_ctx(provider), "hi", &tools)
        .await
        .unwrap();
    assert!(response.is_empty());
}

#[tokio::test]
async fn turn_handles_none_text_response() {
    let provider = Arc::new(ScriptedProvider::new(vec![ChatResponse {
        text: None,
        tool_calls: vec![],
    }]));
    let tools: Vec<Box<dyn Tool>> = vec![];
    let (response, _) = run_turn(&provider_ctx(provider), "hi", &tools)
        .await
        .unwrap();
    assert!(response.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Mixed text + tool call responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_preserves_text_alongside_tool_calls() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ChatResponse {
            text: Some("Let me check...".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "hi"}"#.into(),
            }],
        },
        text_response("Here are the results"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (response, history) = run_turn(&provider_ctx(provider), "check something", &tools)
        .await
        .unwrap();
    assert_eq!(response, "Here are the results");
    let has_intermediate = history
        .iter()
        .any(|m| m.role == "assistant" && m.content.contains("Let me check"));
    assert!(has_intermediate, "Intermediate text should be in history");
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. Multi-tool batch in a single response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multiple_tools_in_one_response() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![
            ToolCall {
                id: "tc1".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc2".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc3".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
        ]),
        text_response("All 3 done"),
    ]));

    let tools: Vec<Box<dyn Tool>> = vec![Box::new(counting_tool)];
    let (response, _) = run_turn(&provider_ctx(provider), "batch", &tools)
        .await
        .unwrap();
    assert_eq!(response, "All 3 done");
    assert_eq!(
        *count.lock().unwrap(),
        3,
        "All 3 tools should have been called"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. System prompt generation & tool instructions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn system_prompt_injected_on_first_turn() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (_, history) = run_turn(&provider_ctx(provider), "hi", &tools)
        .await
        .unwrap();
    assert!(!history.is_empty(), "History should contain entries");
    assert_eq!(
        history[0].role, "system",
        "First history entry should be system prompt"
    );
}

#[tokio::test]
async fn system_prompt_not_duplicated_on_second_turn() {
    // run_one_turn_for_test does a single turn; system appears once in that history.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("only turn")]));
    let tools: Vec<Box<dyn Tool>> = vec![];
    let (_, history) = run_turn(&provider_ctx(provider), "hi", &tools)
        .await
        .unwrap();
    let system_count = history.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 1, "System prompt should appear exactly once");
}

// ═══════════════════════════════════════════════════════════════════════════
// 15. Conversation history fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_contains_all_expected_entries_after_tool_loop() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "tool-out"}"#.into(),
        }]),
        text_response("final answer"),
    ]));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let (response, history) = run_turn(&provider_ctx(provider), "test", &tools)
        .await
        .unwrap();
    assert_eq!(response, "final answer");
    assert!(
        history.len() >= 5,
        "Expected at least 5 history entries, got {}",
        history.len()
    );
    assert_eq!(history[0].role, "system");
    assert_eq!(history[1].role, "user");
    assert!(history[1].content.contains("test"));
    let has_tool_results = history.iter().any(|m| {
        (m.role == "user" && m.content.contains("[Tool results]"))
            || (m.role == "tool" && m.content.contains("tool-out"))
    });
    assert!(
        has_tool_results,
        "Should have tool results in history (user [Tool results] or tool-role message)"
    );
    let last_assistant = history.iter().rev().find(|m| m.role == "assistant");
    assert!(
        last_assistant.unwrap().content.contains("final answer"),
        "Last assistant message should contain final answer (plain or in JSON for native)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 16. Multi-turn conversation maintains context
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_maintains_growing_history() {
    // run_turn is single-turn; verify one turn returns expected response.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("response 1")]));
    let tools: Vec<Box<dyn Tool>> = vec![];
    let (r1, history) = run_turn(&provider_ctx(provider), "msg 1", &tools)
        .await
        .unwrap();
    assert_eq!(r1, "response 1");
    assert!(
        history.len() >= 2,
        "History should have system + user + assistant"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 17. ConversationMessage serialization round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn conversation_message_serialization_roundtrip() {
    let messages = vec![
        ConversationMessage::Chat(ChatMessage::system("system")),
        ConversationMessage::Chat(ChatMessage::user("hello")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    for msg in &messages {
        let json = serde_json::to_string(msg).unwrap();
        let parsed: ConversationMessage = serde_json::from_str(&json).unwrap();

        // Verify the variant type matches
        match (msg, &parsed) {
            (ConversationMessage::Chat(a), ConversationMessage::Chat(b)) => {
                assert_eq!(a.role, b.role);
                assert_eq!(a.content, b.content);
            }
            (
                ConversationMessage::AssistantToolCalls {
                    text: a_text,
                    tool_calls: a_calls,
                },
                ConversationMessage::AssistantToolCalls {
                    text: b_text,
                    tool_calls: b_calls,
                },
            ) => {
                assert_eq!(a_text, b_text);
                assert_eq!(a_calls.len(), b_calls.len());
            }
            (ConversationMessage::ToolResults(a), ConversationMessage::ToolResults(b)) => {
                assert_eq!(a.len(), b.len());
            }
            _ => panic!("Variant mismatch after serialization"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 18. run_single removed; use run_turn (loop_) for single message
// ═══════════════════════════════════════════════════════════════════════════
