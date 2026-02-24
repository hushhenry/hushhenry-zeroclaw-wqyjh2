//! Google Gemini CLI provider (Cloud Code Assist OAuth).
//! Uses Cloud Code Assist API at https://cloudcode-pa.googleapis.com.
//! Credential from providers.json: { "token": "...", "project_id": "..." } (GeminiCli).

use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, TokenUsage, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CLOUDCODE_BASE: &str = "https://cloudcode-pa.googleapis.com";

/// Parsed credential for Gemini CLI (from providers.json GeminiCli or explicit JSON string).
#[derive(Debug, Clone)]
pub struct GeminiCliCredential {
    pub token: String,
    pub project_id: String,
}

/// Parse credential string from store (JSON with "token" and "projectId" or "project_id").
pub fn parse_gemini_cli_credential(s: &str) -> Option<GeminiCliCredential> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let token = v.get("token")?.as_str()?.trim();
    let project_id = v
        .get("projectId")
        .or_else(|| v.get("project_id"))
        .and_then(|v| v.as_str())
        .map(str::trim);
    if token.is_empty() {
        return None;
    }
    let project_id = project_id.unwrap_or("").to_string();
    if project_id.is_empty() {
        return None;
    }
    Some(GeminiCliCredential {
        token: token.to_string(),
        project_id,
    })
}

pub struct GeminiCliProvider {
    credential: Option<GeminiCliCredential>,
    client: Client,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest {
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    generation_config: GenerationConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDeclaration>>,
}

#[derive(Debug, Serialize)]
struct Content {
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize)]
struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<FunctionCallPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<FunctionResponsePart>,
}

#[derive(Debug, Serialize)]
struct FunctionCallPart {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct FunctionResponsePart {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ToolDeclaration {
    function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct FunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    temperature: f64,
    max_output_tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    thoughts_token_count: Option<u64>,
    total_token_count: Option<u64>,
    cached_content_token_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    text: Option<String>,
    function_call: Option<FunctionCallResponse>,
}

#[derive(Debug, Deserialize)]
struct FunctionCallResponse {
    name: String,
    args: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

/// Map Gemini CLI (Cloud Code Assist) usage_metadata to provider TokenUsage.
fn gemini_cli_usage_from_metadata(um: &UsageMetadata) -> Option<TokenUsage> {
    let prompt = um.prompt_token_count.unwrap_or(0);
    let cached = um.cached_content_token_count.unwrap_or(0);
    let input_tokens = prompt.saturating_sub(cached);
    let output_tokens = um.candidates_token_count.unwrap_or(0)
        .saturating_add(um.thoughts_token_count.unwrap_or(0));
    (input_tokens > 0 || output_tokens > 0).then_some(TokenUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
    })
}

impl GeminiCliProvider {
    pub fn new(credential: Option<GeminiCliCredential>) -> Self {
        Self {
            credential,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn format_model(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    fn build_tool_declarations(tools: Option<&[ToolSpec]>) -> Option<Vec<ToolDeclaration>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        Some(vec![ToolDeclaration {
            function_declarations: items
                .iter()
                .map(|t| FunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                })
                .collect(),
        }])
    }

    fn convert_messages(messages: &[ChatMessage]) -> (Option<Content>, Vec<Content>) {
        let mut system_instruction = None;
        let mut contents = Vec::new();
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        for msg in messages {
            if msg.role == "system" {
                system_instruction = Some(Content {
                    role: None,
                    parts: vec![Part {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    }],
                });
                continue;
            }

            if msg.role == "user" {
                contents.push(Content {
                    role: Some("user".to_string()),
                    parts: vec![Part {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    }],
                });
                continue;
            }

            if msg.role == "assistant" {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    if let Some(tool_calls_value) = value.get("tool_calls") {
                        if let Ok(parsed) = serde_json::from_value::<Vec<ProviderToolCall>>(
                            tool_calls_value.clone(),
                        ) {
                            let mut parts = Vec::new();
                            if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
                                if !content.is_empty() {
                                    parts.push(Part {
                                        text: Some(content.to_string()),
                                        function_call: None,
                                        function_response: None,
                                    });
                                }
                            }
                            for call in &parsed {
                                tool_name_by_id.insert(call.id.clone(), call.name.clone());
                                let args: serde_json::Value = serde_json::from_str(&call.arguments)
                                    .unwrap_or_else(|_| serde_json::json!({}));
                                parts.push(Part {
                                    text: None,
                                    function_call: Some(FunctionCallPart {
                                        name: call.name.clone(),
                                        args,
                                    }),
                                    function_response: None,
                                });
                            }
                            contents.push(Content {
                                role: Some("model".to_string()),
                                parts,
                            });
                            continue;
                        }
                    }
                }
                contents.push(Content {
                    role: Some("model".to_string()),
                    parts: vec![Part {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    }],
                });
                continue;
            }

            if msg.role == "tool" {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    let tool_name = value
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| tool_name_by_id.get(id))
                        .cloned();
                    let content = value
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| msg.content.clone());
                    if let Some(name) = tool_name {
                        contents.push(Content {
                            role: Some("user".to_string()),
                            parts: vec![Part {
                                text: None,
                                function_call: None,
                                function_response: Some(FunctionResponsePart {
                                    name,
                                    response: serde_json::json!({ "result": content }),
                                }),
                            }],
                        });
                        continue;
                    }
                }
                contents.push(Content {
                    role: Some("user".to_string()),
                    parts: vec![Part {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    }],
                });
            }
        }

        (system_instruction, contents)
    }
}

#[async_trait]
impl Provider for GeminiCliProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
        }
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> crate::providers::traits::ToolsPayload {
        if tools.is_empty() {
            return crate::providers::traits::ToolsPayload::PromptGuided {
                instructions: crate::providers::traits::build_tool_instructions_text(tools),
            };
        }
        let function_declarations = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters
                })
            })
            .collect();
        crate::providers::traits::ToolsPayload::Gemini {
            function_declarations,
        }
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let cred = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Gemini CLI credential not set. Add token and project_id to config_dir/providers.json under \"gemini-cli\"."
            )
        })?;

        let (system_instruction, contents) = Self::convert_messages(request.messages);
        if contents.is_empty() {
            anyhow::bail!("No messages to send to Gemini CLI");
        }

        let model_name = Self::format_model(model);
        let url = format!(
            "{}/v1beta/projects/{}/locations/us-central1/publishers/google/{}:generateContent",
            CLOUDCODE_BASE, cred.project_id, model_name
        );

        let api_request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
            tools: Self::build_tool_declarations(request.tools),
        };

        let response = self
            .client
            .post(&url)
            .bearer_auth(&cred.token)
            .json(&api_request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini CLI API error ({status}): {error_text}");
        }

        let result: GenerateContentResponse = response.json().await?;

        if let Some(err) = result.error {
            anyhow::bail!("Gemini CLI API error: {}", err.message);
        }

        let candidate = result
            .candidates
            .and_then(|c| c.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("No response from Gemini CLI"))?;

        let mut text = None;
        let mut tool_calls = Vec::new();
        for part in candidate.content.parts {
            if let Some(t) = part.text.filter(|s| !s.is_empty()) {
                text = Some(t);
            }
            if let Some(fc) = part.function_call {
                let id = format!("{}_0", fc.name);
                let arguments = fc
                    .args
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(ProviderToolCall {
                    id,
                    name: fc.name,
                    arguments,
                });
            }
        }

        let usage = result.usage_metadata.as_ref().and_then(gemini_cli_usage_from_metadata);

        Ok(ProviderChatResponse {
            text,
            tool_calls,
            usage,
        })
    }
}
