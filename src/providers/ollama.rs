use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest,
    ChatResponse as ProviderChatResponse, Provider, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct OllamaProvider {
    base_url: String,
    api_key: Option<String>,
    client: Client,
}

// ─── Request Structures ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    options: Options,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OllamaToolSpec>>,
}

#[derive(Debug, Serialize)]
struct OllamaToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: OllamaToolFunctionSpec,
}

#[derive(Debug, Serialize)]
struct OllamaToolFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutgoingToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutgoingToolCall {
    #[serde(rename = "type")]
    kind: String,
    function: OutgoingFunction,
}

#[derive(Debug, Serialize)]
struct OutgoingFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Options {
    temperature: f64,
}

// ─── Response Structures ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiChatResponse {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
    /// Some models return a "thinking" field with internal reasoning
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    id: Option<String>,
    function: OllamaFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

// ─── Implementation ───────────────────────────────────────────────────────────

impl OllamaProvider {
    pub fn new(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        let api_key = api_key.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });

        Self {
            base_url: base_url
                .unwrap_or("http://localhost:11434")
                .trim_end_matches('/')
                .to_string(),
            api_key,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn is_local_endpoint(&self) -> bool {
        reqwest::Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"))
    }

    fn resolve_request_details(&self, model: &str) -> anyhow::Result<(String, bool)> {
        let requests_cloud = model.ends_with(":cloud");
        let normalized_model = model.strip_suffix(":cloud").unwrap_or(model).to_string();

        if requests_cloud && self.is_local_endpoint() {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but Ollama endpoint is local. Configure api_url with a remote Ollama endpoint.",
                model
            );
        }

        if requests_cloud && self.api_key.is_none() {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but no API key is configured. Set OLLAMA_API_KEY or config api_key.",
                model
            );
        }

        let should_auth = self.api_key.is_some() && !self.is_local_endpoint();

        Ok((normalized_model, should_auth))
    }

    fn convert_tools_to_ollama(tools: Option<&[ToolSpec]>) -> Option<Vec<OllamaToolSpec>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        Some(
            items
                .iter()
                .map(|t| OllamaToolSpec {
                    kind: "function".to_string(),
                    function: OllamaToolFunctionSpec {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect(),
        )
    }

    /// Convert ChatMessage history to Ollama Message format.
    /// run_tool_call_loop stores assistant/tool entries as JSON in ChatMessage.content;
    /// we decode those so follow-up requests send structured tool_calls and tool_name.
    fn convert_messages(messages: &[ChatMessage]) -> Vec<Message> {
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        messages
            .iter()
            .map(|m| {
                if m.role == "assistant" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                        if let Some(tool_calls_value) = value.get("tool_calls") {
                            if let Ok(parsed_calls) =
                                serde_json::from_value::<Vec<ProviderToolCall>>(
                                    tool_calls_value.clone(),
                                )
                            {
                                let outgoing_calls: Vec<OutgoingToolCall> = parsed_calls
                                    .iter()
                                    .map(|call| {
                                        tool_name_by_id
                                            .insert(call.id.clone(), call.name.clone());
                                        let args: serde_json::Value = serde_json::from_str(
                                            &call.arguments,
                                        )
                                        .unwrap_or_else(|_| serde_json::json!({}));
                                        OutgoingToolCall {
                                            kind: "function".to_string(),
                                            function: OutgoingFunction {
                                                name: call.name.clone(),
                                                arguments: args,
                                            },
                                        }
                                    })
                                    .collect();
                                let content = value
                                    .get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(String::from)
                                    .unwrap_or_default();
                                return Message {
                                    role: "assistant".to_string(),
                                    content,
                                    tool_calls: Some(outgoing_calls),
                                    tool_name: None,
                                };
                            }
                        }
                    }
                }

                if m.role == "tool" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                        let tool_name = value
                            .get("tool_call_id")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|id| tool_name_by_id.get(id))
                            .cloned();
                        let content = value
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                            .unwrap_or_else(|| m.content.clone());
                        return Message {
                            role: "tool".to_string(),
                            content,
                            tool_calls: None,
                            tool_name,
                        };
                    }
                }

                Message {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    tool_calls: None,
                    tool_name: None,
                }
            })
            .collect()
    }

    /// Send a request to Ollama and get the parsed response.
    /// Pass tools to enable native function-calling for models that support it.
    async fn send_request(
        &self,
        messages: Vec<Message>,
        model: &str,
        temperature: f64,
        should_auth: bool,
        tools: Option<Vec<OllamaToolSpec>>,
    ) -> anyhow::Result<ApiChatResponse> {
        let request = ChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
            options: Options { temperature },
            tools,
        };

        let url = format!("{}/api/chat", self.base_url);

        tracing::debug!(
            "Ollama request: url={} model={} message_count={} temperature={}",
            url,
            model,
            request.messages.len(),
            temperature
        );

        let mut request_builder = self.client.post(&url).json(&request);

        if should_auth {
            if let Some(key) = self.api_key.as_ref() {
                request_builder = request_builder.bearer_auth(key);
            }
        }

        let response = request_builder.send().await?;
        let status = response.status();
        tracing::debug!("Ollama response status: {}", status);

        let body = response.bytes().await?;
        tracing::debug!("Ollama response body length: {} bytes", body.len());

        if !status.is_success() {
            let raw = String::from_utf8_lossy(&body);
            let sanitized = super::sanitize_api_error(&raw);
            tracing::error!(
                "Ollama error response: status={} body_excerpt={}",
                status,
                sanitized
            );
            anyhow::bail!(
                "Ollama API error ({}): {}. Is Ollama running? (brew install ollama && ollama serve)",
                status,
                sanitized
            );
        }

        let chat_response: ApiChatResponse = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                let raw = String::from_utf8_lossy(&body);
                let sanitized = super::sanitize_api_error(&raw);
                tracing::error!(
                    "Ollama response deserialization failed: {e}. body_excerpt={}",
                    sanitized
                );
                anyhow::bail!("Failed to parse Ollama response: {e}");
            }
        };

        Ok(chat_response)
    }

    /// Convert Ollama tool calls to the JSON format expected by parse_tool_calls in loop_.rs
    ///
    /// Handles quirky model behavior where tool calls are wrapped:
    /// - `{"name": "tool_call", "arguments": {"name": "shell", "arguments": {...}}}`
    /// - `{"name": "tool.shell", "arguments": {...}}`
    fn format_tool_calls_for_loop(&self, tool_calls: &[OllamaToolCall]) -> String {
        let formatted_calls: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|tc| {
                let (tool_name, tool_args) = self.extract_tool_name_and_args(tc);

                // Arguments must be a JSON string for parse_tool_calls compatibility
                let args_str =
                    serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".to_string());

                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": args_str
                    }
                })
            })
            .collect();

        serde_json::json!({
            "content": "",
            "tool_calls": formatted_calls
        })
        .to_string()
    }

    /// Extract the actual tool name and arguments from potentially nested structures
    fn extract_tool_name_and_args(&self, tc: &OllamaToolCall) -> (String, serde_json::Value) {
        let name = &tc.function.name;
        let args = &tc.function.arguments;

        // Pattern 1: Nested tool_call wrapper (various malformed versions)
        // {"name": "tool_call", "arguments": {"name": "shell", "arguments": {"command": "date"}}}
        // {"name": "tool_call><json", "arguments": {"name": "shell", ...}}
        // {"name": "tool.call", "arguments": {"name": "shell", ...}}
        if name == "tool_call"
            || name == "tool.call"
            || name.starts_with("tool_call>")
            || name.starts_with("tool_call<")
        {
            if let Some(nested_name) = args.get("name").and_then(|v| v.as_str()) {
                let nested_args = args
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                tracing::debug!(
                    "Unwrapped nested tool call: {} -> {} with args {:?}",
                    name,
                    nested_name,
                    nested_args
                );
                return (nested_name.to_string(), nested_args);
            }
        }

        // Pattern 2: Prefixed tool name (tool.shell, tool.file_read, etc.)
        if let Some(stripped) = name.strip_prefix("tool.") {
            return (stripped.to_string(), args.clone());
        }

        // Pattern 3: Normal tool call
        (name.clone(), args.clone())
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            ..Default::default()
        }
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;
        let api_messages = Self::convert_messages(request.messages);
        let api_tools = Self::convert_tools_to_ollama(request.tools);

        let response = self
            .send_request(api_messages, &normalized_model, temperature, should_auth, api_tools)
            .await?;

        if !response.message.tool_calls.is_empty() {
            tracing::debug!(
                "Ollama returned {} tool call(s)",
                response.message.tool_calls.len()
            );
            let tool_calls: Vec<ProviderToolCall> = response
                .message
                .tool_calls
                .iter()
                .map(|tc| {
                    let (name, args) = self.extract_tool_name_and_args(tc);
                    ProviderToolCall {
                        id: tc.id
                            .clone()
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                    }
                })
                .collect();
            let text = if response.message.content.is_empty() {
                None
            } else {
                Some(response.message.content.clone())
            };
            return Ok(ProviderChatResponse {
                text,
                tool_calls,
            });
        }

        let content = response.message.content;
        if content.is_empty() {
            if let Some(thinking) = &response.message.thinking {
                tracing::warn!(
                    "Ollama returned empty content with only thinking: '{}'. Model may have stopped prematurely.",
                    if thinking.len() > 100 { &thinking[..100] } else { thinking }
                );
                return Ok(ProviderChatResponse {
                    text: Some(format!(
                        "I was thinking about this: {}... but I didn't complete my response. Could you try asking again?",
                        if thinking.len() > 200 { &thinking[..200] } else { thinking }
                    )),
                    tool_calls: vec![],
                });
            }
            tracing::warn!("Ollama returned empty content with no tool calls");
        }

        Ok(ProviderChatResponse {
            text: Some(content),
            tool_calls: vec![],
        })
    }

    fn supports_native_tools(&self) -> bool {
        true
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url() {
        let p = OllamaProvider::new(None, None);
        assert_eq!(p.base_url, "http://localhost:11434");
    }

    #[test]
    fn custom_url_trailing_slash() {
        let p = OllamaProvider::new(Some("http://192.168.1.100:11434/"), None);
        assert_eq!(p.base_url, "http://192.168.1.100:11434");
    }

    #[test]
    fn custom_url_no_trailing_slash() {
        let p = OllamaProvider::new(Some("http://myserver:11434"), None);
        assert_eq!(p.base_url, "http://myserver:11434");
    }

    #[test]
    fn empty_url_uses_empty() {
        let p = OllamaProvider::new(Some(""), None);
        assert_eq!(p.base_url, "");
    }

    #[test]
    fn cloud_suffix_strips_model_name() {
        let p = OllamaProvider::new(Some("https://ollama.com"), Some("ollama-key"));
        let (model, should_auth) = p.resolve_request_details("qwen3:cloud").unwrap();
        assert_eq!(model, "qwen3");
        assert!(should_auth);
    }

    #[test]
    fn cloud_suffix_with_local_endpoint_errors() {
        let p = OllamaProvider::new(None, Some("ollama-key"));
        let error = p
            .resolve_request_details("qwen3:cloud")
            .expect_err("cloud suffix should fail on local endpoint");
        assert!(error
            .to_string()
            .contains("requested cloud routing, but Ollama endpoint is local"));
    }

    #[test]
    fn cloud_suffix_without_api_key_errors() {
        let p = OllamaProvider::new(Some("https://ollama.com"), None);
        let error = p
            .resolve_request_details("qwen3:cloud")
            .expect_err("cloud suffix should require API key");
        assert!(error
            .to_string()
            .contains("requested cloud routing, but no API key is configured"));
    }

    #[test]
    fn remote_endpoint_auth_enabled_when_key_present() {
        let p = OllamaProvider::new(Some("https://ollama.com"), Some("ollama-key"));
        let (_model, should_auth) = p.resolve_request_details("qwen3").unwrap();
        assert!(should_auth);
    }

    #[test]
    fn local_endpoint_auth_disabled_even_with_key() {
        let p = OllamaProvider::new(None, Some("ollama-key"));
        let (_model, should_auth) = p.resolve_request_details("llama3").unwrap();
        assert!(!should_auth);
    }

    #[test]
    fn response_deserializes() {
        let json = r#"{"message":{"role":"assistant","content":"Hello from Ollama!"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hello from Ollama!");
    }

    #[test]
    fn response_with_empty_content() {
        let json = r#"{"message":{"role":"assistant","content":""}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
    }

    #[test]
    fn response_with_missing_content_defaults_to_empty() {
        let json = r#"{"message":{"role":"assistant"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
    }

    #[test]
    fn response_with_thinking_field_extracts_content() {
        let json =
            r#"{"message":{"role":"assistant","content":"hello","thinking":"internal reasoning"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "hello");
    }

    #[test]
    fn response_with_tool_calls_parses_correctly() {
        let json = r#"{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_123","function":{"name":"shell","arguments":{"command":"date"}}}]}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.message.content.is_empty());
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].function.name, "shell");
    }

    #[test]
    fn extract_tool_name_handles_nested_tool_call() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "tool_call".into(),
                arguments: serde_json::json!({
                    "name": "shell",
                    "arguments": {"command": "date"}
                }),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "shell");
        assert_eq!(args.get("command").unwrap(), "date");
    }

    #[test]
    fn extract_tool_name_handles_prefixed_name() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "tool.shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "shell");
        assert_eq!(args.get("command").unwrap(), "ls");
    }

    #[test]
    fn extract_tool_name_handles_normal_call() {
        let provider = OllamaProvider::new(None, None);
        let tc = OllamaToolCall {
            id: Some("call_123".into()),
            function: OllamaFunction {
                name: "file_read".into(),
                arguments: serde_json::json!({"path": "/tmp/test"}),
            },
        };
        let (name, args) = provider.extract_tool_name_and_args(&tc);
        assert_eq!(name, "file_read");
        assert_eq!(args.get("path").unwrap(), "/tmp/test");
    }

    #[test]
    fn format_tool_calls_produces_valid_json() {
        let provider = OllamaProvider::new(None, None);
        let tool_calls = vec![OllamaToolCall {
            id: Some("call_abc".into()),
            function: OllamaFunction {
                name: "shell".into(),
                arguments: serde_json::json!({"command": "date"}),
            },
        }];

        let formatted = provider.format_tool_calls_for_loop(&tool_calls);
        let parsed: serde_json::Value = serde_json::from_str(&formatted).unwrap();

        assert!(parsed.get("tool_calls").is_some());
        let calls = parsed.get("tool_calls").unwrap().as_array().unwrap();
        assert_eq!(calls.len(), 1);

        let func = calls[0].get("function").unwrap();
        assert_eq!(func.get("name").unwrap(), "shell");
        // arguments should be a string (JSON-encoded)
        assert!(func.get("arguments").unwrap().is_string());
    }

    #[test]
    fn convert_messages_parses_native_assistant_tool_calls() {
        use crate::providers::traits::ChatMessage;

        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: r#"{"content":null,"tool_calls":[{"id":"call_1","name":"shell","arguments":"{\"command\":\"ls\"}"}]}"#
                .into(),
        }];

        let converted = OllamaProvider::convert_messages(&messages);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[0].content, "");
        let calls = converted[0].tool_calls.as_ref().expect("tool_calls expected");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "shell");
        assert_eq!(
            calls[0].function.arguments.get("command").and_then(|v| v.as_str()),
            Some("ls")
        );
    }

    #[test]
    fn convert_messages_maps_tool_result_to_tool_name() {
        use crate::providers::traits::ChatMessage;

        let messages = vec![
            ChatMessage {
                role: "assistant".into(),
                content: r#"{"content":null,"tool_calls":[{"id":"call_7","name":"file_read","arguments":"{\"path\":\"README.md\"}"}]}"#
                    .into(),
            },
            ChatMessage {
                role: "tool".into(),
                content: r#"{"tool_call_id":"call_7","content":"ok"}"#.into(),
            },
        ];

        let converted = OllamaProvider::convert_messages(&messages);

        assert_eq!(converted.len(), 2);
        assert_eq!(converted[1].role, "tool");
        assert_eq!(converted[1].tool_name.as_deref(), Some("file_read"));
        assert_eq!(converted[1].content, "ok");
    }

    #[test]
    fn capabilities_include_native_tools() {
        let provider = OllamaProvider::new(None, None);
        let caps = provider.capabilities();
        assert!(caps.native_tool_calling);
    }
}
