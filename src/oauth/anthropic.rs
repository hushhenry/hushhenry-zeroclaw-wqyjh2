//! Anthropic OAuth (Claude Pro/Max).

use super::pkce::generate_pkce;
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthPrompt, OAuthProvider};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use std::collections::HashMap;

const CLIENT_ID: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

fn decode_client_id() -> String {
    String::from_utf8(STANDARD.decode(CLIENT_ID).unwrap_or_default()).unwrap_or_default()
}

/// Anthropic OAuth provider (Claude Pro/Max subscription).
pub struct AnthropicOAuthProvider;

#[async_trait]
impl OAuthProvider for AnthropicOAuthProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials> {
        let client_id = decode_client_id();
        let pkce = generate_pkce();

        let params = [
            ("code", "true"),
            ("client_id", &client_id),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPES),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", &pkce.verifier),
        ];

        let auth_url = format!(
            "{}?{}",
            AUTHORIZE_URL,
            serde_urlencoded::to_string(&params)?
        );

        callbacks.on_auth(OAuthAuthInfo {
            url: auth_url.clone(),
            instructions: Some(
                "Complete sign-in in your browser, then paste the authorization code.".into(),
            ),
        });

        let auth_code = callbacks
            .on_prompt(OAuthPrompt {
                message: "Paste the authorization code (format: code#state):".into(),
                placeholder: None,
            })
            .await?;

        let splits: Vec<&str> = auth_code.split('#').collect();
        let code = splits.first().unwrap_or(&"").to_string();
        let state = splits.get(1).unwrap_or(&"").to_string();

        callbacks.on_progress("Exchanging authorization code for tokens...");

        let client = reqwest::Client::new();
        let resp = client
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "code": code,
                "state": state,
                "redirect_uri": REDIRECT_URI,
                "code_verifier": pkce.verifier,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await?;
            anyhow::bail!("Token exchange failed: {}", body);
        }

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            refresh_token: String,
            expires_in: i64,
        }

        let token: TokenResp = resp.json().await?;
        let expires =
            chrono::Utc::now().timestamp_millis() + token.expires_in * 1000 - 5 * 60 * 1000;

        Ok(OAuthCredentials {
            refresh: token.refresh_token,
            access: token.access_token,
            expires,
            extra: HashMap::new(),
        })
    }

    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        let client_id = decode_client_id();
        let client = reqwest::Client::new();

        let resp = client
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": client_id,
                "refresh_token": credentials.refresh,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await?;
            anyhow::bail!("Anthropic token refresh failed: {}", body);
        }

        #[derive(Deserialize)]
        struct RefreshResp {
            access_token: String,
            refresh_token: String,
            expires_in: i64,
        }

        let data: RefreshResp = resp.json().await?;
        let expires =
            chrono::Utc::now().timestamp_millis() + data.expires_in * 1000 - 5 * 60 * 1000;

        Ok(OAuthCredentials {
            refresh: data.refresh_token,
            access: data.access_token,
            expires,
            extra: HashMap::new(),
        })
    }

    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        credentials.access.clone()
    }
}
