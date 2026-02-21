//! Google Gemini provider with support for:
//! - Direct API key (`GEMINI_API_KEY` env var or config)
//! - Gemini CLI OAuth tokens (reuse existing ~/.gemini/ authentication)
//! - Native function calling (tool declarations + function_call / function_response in parts)

use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ToolCall as ProviderToolCall,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use directories::UserDirs;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Gemini provider supporting multiple authentication methods.
pub struct GeminiProvider {
    auth: Option<GeminiAuth>,
    client: Client,
}

/// Resolved credential — the variant determines both the HTTP auth method
/// and the diagnostic label returned by `auth_source()`.
#[derive(Debug)]
enum GeminiAuth {
    /// Explicit API key from config: sent as `?key=` query parameter.
    ExplicitKey(String),
    /// API key from `GEMINI_API_KEY` env var: sent as `?key=`.
    EnvGeminiKey(String),
    /// API key from `GOOGLE_API_KEY` env var: sent as `?key=`.
    EnvGoogleKey(String),
    /// OAuth access token from Gemini CLI: sent as `Authorization: Bearer`.
    OAuthToken(String),
}

impl GeminiAuth {
    /// Whether this credential is an API key (sent as `?key=` query param).
    fn is_api_key(&self) -> bool {
        matches!(
            self,
            GeminiAuth::ExplicitKey(_) | GeminiAuth::EnvGeminiKey(_) | GeminiAuth::EnvGoogleKey(_)
        )
    }

    /// The raw credential string.
    fn credential(&self) -> &str {
        match self {
            GeminiAuth::ExplicitKey(s)
            | GeminiAuth::EnvGeminiKey(s)
            | GeminiAuth::EnvGoogleKey(s)
            | GeminiAuth::OAuthToken(s) => s,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// API REQUEST/RESPONSE TYPES
// ══════════════════════════════════════════════════════════════════════════════

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
#[serde(rename_all = "camelCase")]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

impl CandidateContent {
    /// Extract effective text, skipping thinking and thought-signature-only parts.
    ///
    /// Prefers non-thinking text; falls back to the first thinking segment when no
    /// non-thinking content is present (so thinking-only responses still yield text).
    fn effective_text(&self) -> Option<String> {
        let mut answer_parts: Vec<&str> = Vec::new();
        let mut first_thinking: Option<&str> = None;

        for part in &self.parts {
            let Some(ref text) = part.text else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if !part.thought.unwrap_or(false) {
                answer_parts.push(text);
            } else if first_thinking.is_none() {
                first_thinking = Some(text);
            }
        }

        if answer_parts.is_empty() {
            first_thinking.map(String::from)
        } else {
            Some(answer_parts.join(""))
        }
    }
}

/// Part of a Gemini candidate content.
/// Thinking models (e.g. gemini-2.5-flash) can return:
/// - `{"thought": true, "text": "reasoning..."}` — internal reasoning
/// - `{"text": "actual answer"}` — the response
/// - `{"thoughtSignature": "..."}` — opaque signature (no text); deserialized and skipped for display
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    /// Opaque thought signature from Gemini thinking models; accepted and skipped when building effective text.
    #[serde(rename = "thoughtSignature", default)]
    thought_signature: Option<String>,
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

// ══════════════════════════════════════════════════════════════════════════════
// GEMINI CLI TOKEN STRUCTURES
// ══════════════════════════════════════════════════════════════════════════════

/// OAuth token stored by Gemini CLI in `~/.gemini/oauth_creds.json`
#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCreds {
    access_token: Option<String>,
    expiry: Option<String>,
}

impl GeminiProvider {
    /// Create a new Gemini provider.
    ///
    /// Authentication priority:
    /// 1. Explicit API key passed in
    /// 2. `GEMINI_API_KEY` environment variable
    /// 3. `GOOGLE_API_KEY` environment variable
    /// 4. Gemini CLI OAuth tokens (`~/.gemini/oauth_creds.json`)
    pub fn new(api_key: Option<&str>) -> Self {
        let resolved_auth = api_key
            .and_then(Self::normalize_non_empty)
            .map(GeminiAuth::ExplicitKey)
            .or_else(|| Self::load_non_empty_env("GEMINI_API_KEY").map(GeminiAuth::EnvGeminiKey))
            .or_else(|| Self::load_non_empty_env("GOOGLE_API_KEY").map(GeminiAuth::EnvGoogleKey))
            .or_else(|| Self::try_load_gemini_cli_token().map(GeminiAuth::OAuthToken));

        Self {
            auth: resolved_auth,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn normalize_non_empty(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn load_non_empty_env(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .and_then(|value| Self::normalize_non_empty(&value))
    }

    /// Try to load OAuth access token from Gemini CLI's cached credentials.
    /// Location: `~/.gemini/oauth_creds.json`
    fn try_load_gemini_cli_token() -> Option<String> {
        let gemini_dir = Self::gemini_cli_dir()?;
        let creds_path = gemini_dir.join("oauth_creds.json");

        if !creds_path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&creds_path).ok()?;
        let creds: GeminiCliOAuthCreds = serde_json::from_str(&content).ok()?;

        // Check if token is expired (basic check)
        if let Some(ref expiry) = creds.expiry {
            if let Ok(expiry_time) = chrono::DateTime::parse_from_rfc3339(expiry) {
                if expiry_time < chrono::Utc::now() {
                    tracing::warn!("Gemini CLI OAuth token expired — re-run `gemini` to refresh");
                    return None;
                }
            }
        }

        creds
            .access_token
            .and_then(|token| Self::normalize_non_empty(&token))
    }

    /// Get the Gemini CLI config directory (~/.gemini)
    fn gemini_cli_dir() -> Option<PathBuf> {
        UserDirs::new().map(|u| u.home_dir().join(".gemini"))
    }

    /// Check if Gemini CLI is configured and has valid credentials
    pub fn has_cli_credentials() -> bool {
        Self::try_load_gemini_cli_token().is_some()
    }

    /// Check if any Gemini authentication is available
    pub fn has_any_auth() -> bool {
        Self::load_non_empty_env("GEMINI_API_KEY").is_some()
            || Self::load_non_empty_env("GOOGLE_API_KEY").is_some()
            || Self::has_cli_credentials()
    }

    /// Get authentication source description for diagnostics.
    /// Uses the stored enum variant — no env var re-reading at call time.
    pub fn auth_source(&self) -> &'static str {
        match self.auth.as_ref() {
            Some(GeminiAuth::ExplicitKey(_)) => "config",
            Some(GeminiAuth::EnvGeminiKey(_)) => "GEMINI_API_KEY env var",
            Some(GeminiAuth::EnvGoogleKey(_)) => "GOOGLE_API_KEY env var",
            Some(GeminiAuth::OAuthToken(_)) => "Gemini CLI OAuth",
            None => "none",
        }
    }

    fn format_model_name(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    fn build_generate_content_url(model: &str, auth: &GeminiAuth) -> String {
        let model_name = Self::format_model_name(model);
        let base_url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{model_name}:generateContent"
        );

        if auth.is_api_key() {
            format!("{base_url}?key={}", auth.credential())
        } else {
            base_url
        }
    }

    fn build_generate_content_request(
        &self,
        auth: &GeminiAuth,
        url: &str,
        request: &GenerateContentRequest,
    ) -> reqwest::RequestBuilder {
        let req = self.client.post(url).json(request);
        match auth {
            GeminiAuth::OAuthToken(token) => req.bearer_auth(token),
            _ => req,
        }
    }

    /// Convert ToolSpec to Gemini functionDeclarations.
    fn convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<ToolDeclaration>> {
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

    /// Convert ChatMessage history to Gemini contents (role + parts with text / function_call / function_response).
    /// run_tool_call_loop stores assistant/tool as JSON in content; we decode for native tool rounds.
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
                        if let Ok(parsed_calls) = serde_json::from_value::<Vec<ProviderToolCall>>(
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
                            for call in &parsed_calls {
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

static TOOL_CALL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[async_trait]
impl Provider for GeminiProvider {
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
        let auth = self.auth.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Gemini API key not found. Options:\n\
                 1. Set GEMINI_API_KEY env var\n\
                 2. Run `gemini` CLI to authenticate (tokens will be reused)\n\
                 3. Get an API key from https://aistudio.google.com/app/apikey\n\
                 4. Run `zeroclaw onboard` to configure"
            )
        })?;

        let (system_instruction, contents) = Self::convert_messages(request.messages);
        let tools = Self::convert_tools(request.tools);

        if contents.is_empty() {
            anyhow::bail!("No messages to send to Gemini");
        }

        let api_request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
            tools,
        };

        let url = Self::build_generate_content_url(model, auth);

        let response = self
            .build_generate_content_request(auth, &url, &api_request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({status}): {error_text}");
        }

        let result: GenerateContentResponse = response.json().await?;

        if let Some(err) = result.error {
            anyhow::bail!("Gemini API error: {}", err.message);
        }

        let candidate = result
            .candidates
            .and_then(|c| c.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("No response from Gemini"))?;

        let text = candidate.content.effective_text();
        let mut tool_calls = Vec::new();

        for part in candidate.content.parts {
            if let Some(fc) = part.function_call {
                let counter = TOOL_CALL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = format!("{}_{}", fc.name, counter);
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

        Ok(ProviderChatResponse { text, tool_calls })
    }

    fn supports_native_tools(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn normalize_non_empty_trims_and_filters() {
        assert_eq!(
            GeminiProvider::normalize_non_empty(" value "),
            Some("value".into())
        );
        assert_eq!(GeminiProvider::normalize_non_empty(""), None);
        assert_eq!(GeminiProvider::normalize_non_empty(" \t\n"), None);
    }

    #[test]
    fn provider_creates_without_key() {
        let provider = GeminiProvider::new(None);
        // May pick up env vars; just verify it doesn't panic
        let _ = provider.auth_source();
    }

    #[test]
    fn provider_creates_with_key() {
        let provider = GeminiProvider::new(Some("test-api-key"));
        assert!(matches!(
            provider.auth,
            Some(GeminiAuth::ExplicitKey(ref key)) if key == "test-api-key"
        ));
    }

    #[test]
    fn provider_rejects_empty_key() {
        let provider = GeminiProvider::new(Some(""));
        assert!(!matches!(provider.auth, Some(GeminiAuth::ExplicitKey(_))));
    }

    #[test]
    fn gemini_cli_dir_returns_path() {
        let dir = GeminiProvider::gemini_cli_dir();
        // Should return Some on systems with home dir
        if UserDirs::new().is_some() {
            assert!(dir.is_some());
            assert!(dir.unwrap().ends_with(".gemini"));
        }
    }

    #[test]
    fn auth_source_explicit_key() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("key".into())),
            client: Client::new(),
        };
        assert_eq!(provider.auth_source(), "config");
    }

    #[test]
    fn auth_source_none_without_credentials() {
        let provider = GeminiProvider {
            auth: None,
            client: Client::new(),
        };
        assert_eq!(provider.auth_source(), "none");
    }

    #[test]
    fn auth_source_oauth() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock".into())),
            client: Client::new(),
        };
        assert_eq!(provider.auth_source(), "Gemini CLI OAuth");
    }

    #[test]
    fn model_name_formatting() {
        assert_eq!(
            GeminiProvider::format_model_name("gemini-2.0-flash"),
            "models/gemini-2.0-flash"
        );
        assert_eq!(
            GeminiProvider::format_model_name("models/gemini-1.5-pro"),
            "models/gemini-1.5-pro"
        );
    }

    #[test]
    fn api_key_url_includes_key_query_param() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.contains(":generateContent?key=api-key-123"));
    }

    #[test]
    fn oauth_url_omits_key_query_param() {
        let auth = GeminiAuth::OAuthToken("ya29.test-token".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.ends_with(":generateContent"));
        assert!(!url.contains("?key="));
    }

    #[test]
    fn oauth_request_uses_bearer_auth_header() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::OAuthToken("ya29.mock-token".into())),
            client: Client::new(),
        };
        let auth = GeminiAuth::OAuthToken("ya29.mock-token".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part {
                    text: Some("hello".into()),
                    function_call: None,
                    function_response: None,
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let request = provider
            .build_generate_content_request(&auth, &url, &body)
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|h| h.to_str().ok()),
            Some("Bearer ya29.mock-token")
        );
    }

    #[test]
    fn api_key_request_does_not_set_bearer_header() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("api-key-123".into())),
            client: Client::new(),
        };
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part {
                    text: Some("hello".into()),
                    function_call: None,
                    function_response: None,
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let request = provider
            .build_generate_content_request(&auth, &url, &body)
            .build()
            .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn request_serialization() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part {
                    text: Some("Hello".to_string()),
                    function_call: None,
                    function_response: None,
                }],
            }],
            system_instruction: Some(Content {
                role: None,
                parts: vec![Part {
                    text: Some("You are helpful".to_string()),
                    function_call: None,
                    function_response: None,
                }],
            }),
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
            tools: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("user"));
        assert!(json.contains("Hello"));
        assert!(json.contains("temperature"));
        assert!(json.contains("maxOutputTokens"));
    }

    #[test]
    fn response_deserialization() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.candidates.is_some());
        let text = response
            .candidates
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .content
            .parts
            .into_iter()
            .next()
            .unwrap()
            .text;
        assert_eq!(text, Some("Hello there!".to_string()));
    }

    #[test]
    fn response_deserialization_with_function_call() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "I'll check.", "thought": false},
                        {"functionCall": {"name": "shell", "args": {"command": "date"}}}
                    ]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let parts = &response.candidates.as_ref().unwrap()[0].content.parts;
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_deref(), Some("I'll check."));
        let fc = parts[1].function_call.as_ref().unwrap();
        assert_eq!(fc.name, "shell");
        assert_eq!(
            fc.args
                .as_ref()
                .and_then(|a| a.get("command").and_then(|v| v.as_str())),
            Some("date")
        );
    }

    #[test]
    fn response_deserialization_with_thought_signature() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {"thought": true, "text": "reasoning step"},
                        {"text": "Final answer."},
                        {"thoughtSignature": "opaque-sig-abc123"}
                    ]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let content = &response.candidates.as_ref().unwrap()[0].content;
        assert_eq!(content.parts.len(), 3);
        assert_eq!(
            content.parts[2].thought_signature.as_deref(),
            Some("opaque-sig-abc123")
        );
        assert_eq!(content.effective_text().as_deref(), Some("Final answer."));
    }

    #[test]
    fn effective_text_falls_back_to_thinking_when_no_answer_parts() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {"thought": true, "text": "only reasoning"}
                    ]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let content = &response.candidates.as_ref().unwrap()[0].content;
        assert_eq!(content.effective_text().as_deref(), Some("only reasoning"));
    }

    #[test]
    fn effective_text_skips_signature_only_parts() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {"thoughtSignature": "sig-only"}
                    ]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let content = &response.candidates.as_ref().unwrap()[0].content;
        assert_eq!(content.effective_text(), None);
    }

    #[test]
    fn convert_messages_parses_assistant_tool_calls() {
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                content: "Run date".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: r#"{"content":null,"tool_calls":[{"id":"call_1","name":"shell","arguments":"{\"command\":\"date\"}"}]}"#
                    .into(),
            },
        ];

        let (system, contents) = GeminiProvider::convert_messages(&messages);
        assert!(system.is_none());
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1].role.as_deref(), Some("model"));
        let parts = &contents[1].parts;
        assert_eq!(parts.len(), 1);
        assert!(parts[0].function_call.is_some());
        assert_eq!(parts[0].function_call.as_ref().unwrap().name, "shell");
    }

    #[test]
    fn convert_messages_parses_tool_result() {
        let messages = vec![
            ChatMessage {
                role: "assistant".into(),
                content: r#"{"content":null,"tool_calls":[{"id":"call_7","name":"file_read","arguments":"{\"path\":\"a.txt\"}"}]}"#
                    .into(),
            },
            ChatMessage {
                role: "tool".into(),
                content: r#"{"tool_call_id":"call_7","content":"file body"}"#.into(),
            },
        ];

        let (_, contents) = GeminiProvider::convert_messages(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1].role.as_deref(), Some("user"));
        let fr = contents[1].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.name, "file_read");
        assert_eq!(
            fr.response.get("result").and_then(|v| v.as_str()),
            Some("file body")
        );
    }

    #[test]
    fn capabilities_include_native_tools() {
        let provider = GeminiProvider::new(None);
        let caps = provider.capabilities();
        assert!(caps.native_tool_calling);
    }

    #[test]
    fn error_response_deserialization() {
        let json = r#"{
            "error": {
                "message": "Invalid API key"
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().message, "Invalid API key");
    }
}
