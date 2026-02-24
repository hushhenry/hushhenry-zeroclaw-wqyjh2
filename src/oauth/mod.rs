//! OAuth providers and flows (migrated from zeroai for onboard TUI).
//! Supports Anthropic, OpenAI Codex, Google Gemini CLI, Antigravity, GitHub Copilot, Qwen Portal.

pub mod anthropic;
pub mod github_copilot;
pub mod google_antigravity;
pub mod google_gemini_cli;
pub mod openai_codex;
pub mod pkce;
pub mod qwen_portal;
pub mod store;

use async_trait::async_trait;
use std::collections::HashMap;

/// Information about the OAuth authorization URL.
#[derive(Debug, Clone)]
pub struct OAuthAuthInfo {
    pub url: String,
    pub instructions: Option<String>,
}

/// Prompt to show to the user during OAuth.
#[derive(Debug, Clone)]
pub struct OAuthPrompt {
    pub message: String,
    pub placeholder: Option<String>,
}

/// Callbacks for the OAuth login flow.
#[async_trait]
pub trait OAuthCallbacks: Send + Sync {
    /// Called when the user should open a URL in their browser.
    fn on_auth(&self, info: OAuthAuthInfo);
    /// Called when the user needs to input a value (e.g. authorization code).
    async fn on_prompt(&self, prompt: OAuthPrompt) -> anyhow::Result<String>;
    /// Called with progress messages.
    fn on_progress(&self, message: &str);
}

/// Credentials returned from OAuth login.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    /// Expiry timestamp in milliseconds since epoch.
    pub expires: i64,
    /// Extra data (e.g. `projectId` for Google).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Trait for OAuth provider implementations.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Provider ID (e.g. "anthropic", "gemini-cli", "openai-codex").
    fn id(&self) -> &str;

    /// Human-readable name.
    fn name(&self) -> &str;

    /// Run the login flow.
    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials>;

    /// Refresh an expired token.
    async fn refresh_token(&self, credentials: &OAuthCredentials)
        -> anyhow::Result<OAuthCredentials>;

    /// Convert credentials to an API key string for use in config.
    fn get_api_key(&self, credentials: &OAuthCredentials) -> String;
}

/// OAuth provider IDs that can be used in the onboard TUI.
pub const OAUTH_PROVIDER_IDS: &[&str] = &[
    "anthropic",
    "openai-codex",
    "gemini-cli",
    "antigravity",
    "github-copilot",
    "qwen-portal",
];

/// Return the OAuth provider implementation for the given provider id, if any.
pub fn oauth_provider_for_id(id: &str) -> Option<Box<dyn OAuthProvider>> {
    match id {
        "anthropic" => Some(Box::new(anthropic::AnthropicOAuthProvider)),
        "openai-codex" | "codex" => Some(Box::new(openai_codex::OpenAiCodexOAuthProvider)),
        "gemini-cli" => Some(Box::new(google_gemini_cli::GeminiCliOAuthProvider)),
        "antigravity" => Some(Box::new(google_antigravity::AntigravityOAuthProvider)),
        "github-copilot" => Some(Box::new(github_copilot::GitHubCopilotOAuthProvider)),
        "qwen-portal" | "qwen" => Some(Box::new(qwen_portal::QwenPortalOAuthProvider)),
        _ => None,
    }
}

/// Returns true if the provider is OAuth-only (no API key path in TUI; use OAuth flow directly).
pub fn is_oauth_only_provider(id: &str) -> bool {
    matches!(
        id,
        "openai-codex" | "codex" | "gemini-cli" | "antigravity" | "github-copilot"
            | "qwen-portal" | "qwen"
    )
}
