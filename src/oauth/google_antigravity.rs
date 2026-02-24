//! Google Antigravity OAuth (Gemini 3, Claude, GPT-OSS via Google Cloud).

use super::pkce::generate_pkce;
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthPrompt, OAuthProvider};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

const CLIENT_ID_HEX: &str = "313037313030363036303539312d746d687373696e326832316c63726532333576746f6c6f6a68346734303365702e617070732e676f6f676c6575736572636f6e74656e742e636f6d";
const CLIENT_SECRET_HEX: &str = "474f435350582d4b35384657523438364c644c4a316d4c4238735843347a3671444166";

fn get_client_id() -> String {
    let bytes = (0..CLIENT_ID_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&CLIENT_ID_HEX[i..i + 2], 16).unwrap_or_default())
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap_or_default()
}

fn get_client_secret() -> String {
    let bytes = (0..CLIENT_SECRET_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&CLIENT_SECRET_HEX[i..i + 2], 16).unwrap_or_default())
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap_or_default()
}

const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

/// Antigravity OAuth provider (Gemini 3, Claude, GPT-OSS via Google Cloud).
pub struct AntigravityOAuthProvider;

#[async_trait]
impl OAuthProvider for AntigravityOAuthProvider {
    fn id(&self) -> &str {
        "antigravity"
    }

    fn name(&self) -> &str {
        "Antigravity (Gemini 3, Claude, GPT-OSS)"
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials> {
        let client_id = get_client_id();
        let pkce = generate_pkce();

        let scopes = SCOPES.join(" ");
        let params = [
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", &scopes),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", &pkce.verifier),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];

        let auth_url = format!("{}?{}", AUTH_URL, serde_urlencoded::to_string(params)?);

        callbacks.on_auth(OAuthAuthInfo {
            url: auth_url.clone(),
            instructions: Some("Complete the sign-in in your browser (visit the URL below).".into()),
        });

        let redirect_url = callbacks
            .on_prompt(OAuthPrompt {
                message: "Paste the redirect URL from your browser:".into(),
                placeholder: Some("http://localhost:51121/oauth-callback?code=...&state=...".into()),
            })
            .await?;

        let parsed = url::Url::parse(&redirect_url)?;
        let code = parsed
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| anyhow::anyhow!("No authorization code in redirect URL"))?;

        callbacks.on_progress("Exchanging authorization code for tokens...");

        let client_secret = get_client_secret();
        let client = reqwest::Client::new();
        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", &code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &pkce.verifier),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await?;
            anyhow::bail!("Token exchange failed: {}", body);
        }

        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            refresh_token: Option<String>,
            expires_in: i64,
        }

        let token: TokenResp = resp.json().await?;
        let refresh = token
            .refresh_token
            .ok_or_else(|| anyhow::anyhow!("No refresh token received"))?;

        callbacks.on_progress("Discovering project...");
        let project_id = discover_project(&token.access_token, callbacks).await?;

        let expires =
            chrono::Utc::now().timestamp_millis() + token.expires_in * 1000 - 5 * 60 * 1000;

        let mut extra = HashMap::new();
        extra.insert("projectId".into(), serde_json::json!(project_id));

        Ok(OAuthCredentials {
            refresh,
            access: token.access_token,
            expires,
            extra,
        })
    }

    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        let project_id = credentials
            .extra
            .get("projectId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing projectId in credentials"))?
            .to_string();

        let client_id = get_client_id();
        let client_secret = get_client_secret();
        let client = reqwest::Client::new();

        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("refresh_token", &credentials.refresh),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await?;
            anyhow::bail!("Antigravity token refresh failed: {}", body);
        }

        #[derive(Deserialize)]
        struct RefreshResp {
            access_token: String,
            expires_in: i64,
            refresh_token: Option<String>,
        }

        let data: RefreshResp = resp.json().await?;
        let expires =
            chrono::Utc::now().timestamp_millis() + data.expires_in * 1000 - 5 * 60 * 1000;

        let mut extra = HashMap::new();
        extra.insert("projectId".into(), serde_json::json!(project_id));

        Ok(OAuthCredentials {
            refresh: data
                .refresh_token
                .unwrap_or_else(|| credentials.refresh.clone()),
            access: data.access_token,
            expires,
            extra,
        })
    }

    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        let project_id = credentials
            .extra
            .get("projectId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        serde_json::json!({
            "token": credentials.access,
            "projectId": project_id
        })
        .to_string()
    }
}

async fn discover_project(
    access_token: &str,
    callbacks: &dyn OAuthCallbacks,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    let endpoints = [
        "https://cloudcode-pa.googleapis.com",
        "https://daily-cloudcode-pa.sandbox.googleapis.com",
    ];

    callbacks.on_progress("Checking for existing project...");

    for endpoint in &endpoints {
        let resp = client
            .post(format!("{}/v1internal:loadCodeAssist", endpoint))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", "google-cloud-sdk vscode_cloudshelleditor/0.1")
            .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
            .json(&serde_json::json!({
                "metadata": {
                    "ideType": "IDE_UNSPECIFIED",
                    "platform": "PLATFORM_UNSPECIFIED",
                    "pluginType": "GEMINI"
                }
            }))
            .send()
            .await;

        if let Ok(resp) = resp {
            if resp.status().is_success() {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct LoadResp {
                    cloudaicompanion_project: Option<serde_json::Value>,
                }

                if let Ok(data) = resp.json::<LoadResp>().await {
                    if let Some(project) = data.cloudaicompanion_project {
                        match project {
                            serde_json::Value::String(s) if !s.is_empty() => return Ok(s),
                            serde_json::Value::Object(obj) => {
                                if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                                    if !id.is_empty() {
                                        return Ok(id.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    callbacks.on_progress("Using default project...");
    Ok(DEFAULT_PROJECT_ID.to_string())
}
