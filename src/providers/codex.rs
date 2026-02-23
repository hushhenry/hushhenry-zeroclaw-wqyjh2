//! OpenAI Codex provider (ChatGPT backend OAuth).
//! Uses the Responses API at https://chatgpt.com/backend-api/codex/responses.
//! Credential: OAuth access token from providers.json or CODEX_ACCESS_TOKEN / OPENAI_OAUTH_TOKEN.

use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse, Provider,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CODEX_RESPONSES_PATH: &str = "/codex/responses";

pub struct CodexProvider {
    credential: Option<String>,
    client: Client,
}

#[derive(Debug, Serialize)]
struct CodexRequest {
    model: String,
    input: Vec<CodexInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Serialize)]
struct CodexInput {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct CodexResponse {
    #[serde(default)]
    output: Vec<CodexOutput>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexOutput {
    #[serde(default)]
    content: Vec<CodexContent>,
}

#[derive(Debug, Deserialize)]
struct CodexContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

impl CodexProvider {
    pub fn new(credential: Option<&str>) -> Self {
        let credential = credential
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        Self {
            credential,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn messages_to_input(messages: &[ChatMessage]) -> (Option<String>, Vec<CodexInput>) {
        let mut instructions = None;
        let mut input = Vec::new();
        for m in messages {
            if m.role == "system" {
                instructions = Some(m.content.clone());
                continue;
            }
            let role = match m.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "tool" => "user",
                _ => "user",
            };
            input.push(CodexInput {
                role: role.to_string(),
                content: m.content.clone(),
            });
        }
        (instructions, input)
    }

    fn extract_text(response: CodexResponse) -> Option<String> {
        if let Some(t) = response
            .output_text
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            return Some(t.trim().to_string());
        }
        for item in &response.output {
            for c in &item.content {
                if let Some(t) = c.text.as_deref().filter(|s| !s.trim().is_empty()) {
                    return Some(t.trim().to_string());
                }
            }
        }
        None
    }
}

#[async_trait]
impl Provider for CodexProvider {
    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Codex OAuth token not set. Set CODEX_ACCESS_TOKEN or add to config_dir/providers.json."
            )
        })?;

        let (instructions, input) = Self::messages_to_input(request.messages);
        if input.is_empty() {
            anyhow::bail!("No messages to send to Codex");
        }

        let body = CodexRequest {
            model: model.to_string(),
            input,
            instructions,
            stream: Some(false),
        };

        let url = format!("{CODEX_BASE_URL}{CODEX_RESPONSES_PATH}");
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {credential}"))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Codex API error ({status}): {error_text}");
        }

        let parsed: CodexResponse = response.json().await?;
        let text = Self::extract_text(parsed)
            .ok_or_else(|| anyhow::anyhow!("No response text from Codex"))?;

        Ok(ProviderChatResponse {
            text: Some(text),
            tool_calls: vec![],
        })
    }
}
