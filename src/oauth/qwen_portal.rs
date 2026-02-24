//! Qwen Portal OAuth.

use super::pkce::generate_pkce;
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthPrompt, OAuthProvider};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

const CLIENT_ID: &str = "f0304373b74a44d2b584a3fb70ca9e56";
const AUTHORIZE_URL: &str = "https://portal.qwen.ai/oauth/authorize";
const TOKEN_URL: &str = "https://portal.qwen.ai/v1/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Qwen Portal OAuth provider.
pub struct QwenPortalOAuthProvider;

#[async_trait]
impl OAuthProvider for QwenPortalOAuthProvider {
    fn id(&self) -> &str {
        "qwen-portal"
    }
    fn name(&self) -> &str {
        "Qwen Portal OAuth"
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials> {
        let pkce = generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let params = [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "openid profile email"),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ];

        let auth_url = format!(
            "{}?{}",
            AUTHORIZE_URL,
            serde_urlencoded::to_string(&params)?
        );

        callbacks.on_auth(OAuthAuthInfo {
            url: auth_url,
            instructions: Some(
                "Authorization page opened. Login and paste the redirect URL here.".into(),
            ),
        });

        let input = callbacks
            .on_prompt(OAuthPrompt {
                message: "Paste the redirect URL:".into(),
                placeholder: None,
            })
            .await?;

        let parsed = url::Url::parse(&input)?;
        let code = parsed
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| anyhow::anyhow!("Missing code"))?;

        let client = reqwest::Client::new();
        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &code),
                ("code_verifier", &pkce.verifier),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Token exchange failed: {}", resp.text().await?);
        }

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            refresh_token: String,
            expires_in: i64,
        }
        let token: TokenResp = resp.json().await?;
        let expires =
            chrono::Utc::now().timestamp_millis() + token.expires_in * 1000 - 300_000;

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
        let client = reqwest::Client::new();
        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &credentials.refresh),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await?;

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            refresh_token: String,
            expires_in: i64,
        }
        let token: TokenResp = resp.json().await?;
        let expires =
            chrono::Utc::now().timestamp_millis() + token.expires_in * 1000 - 300_000;

        Ok(OAuthCredentials {
            refresh: token.refresh_token,
            access: token.access_token,
            expires,
            extra: HashMap::new(),
        })
    }

    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        credentials.access.clone()
    }
}
