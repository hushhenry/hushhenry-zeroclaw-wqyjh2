//! Google Gemini CLI OAuth (Cloud Code Assist).

use super::pkce::generate_pkce;
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthPrompt, OAuthProvider};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

const CLIENT_ID_HEX: &str = "3638313235353830393339352d6f6f386674326f707264726e7039653361716636617633686d6469623133356a2e617070732e676f6f676c6575736572636f6e74656e742e636f6d";
const CLIENT_SECRET_HEX: &str = "474f435350582d347548674d506d2d316f37536b2d67655636437535636c584673786c";

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

const REDIRECT_URI_OOB: &str = "https://codeassist.google.com/authcode";
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";

/// Google Gemini CLI OAuth provider (Cloud Code Assist).
pub struct GeminiCliOAuthProvider;

#[async_trait]
impl OAuthProvider for GeminiCliOAuthProvider {
    fn id(&self) -> &str {
        "gemini-cli"
    }

    fn name(&self) -> &str {
        "Google Cloud Code Assist (Gemini CLI)"
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials> {
        let client_id = get_client_id();
        let pkce = generate_pkce();

        let scopes = SCOPES.join(" ");
        let params = [
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI_OOB),
            ("scope", &scopes),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];

        let auth_url = format!("{}?{}", AUTH_URL, serde_urlencoded::to_string(&params)?);

        callbacks.on_auth(OAuthAuthInfo {
            url: auth_url.clone(),
            instructions: Some(
                "Visit the URL below in your browser. Paste the code from the success page."
                    .into(),
            ),
        });

        let code = callbacks
            .on_prompt(OAuthPrompt {
                message: "Enter authorization code:".into(),
                placeholder: None,
            })
            .await?;

        callbacks.on_progress("Exchanging code for tokens...");

        let client_secret = get_client_secret();
        let client = reqwest::Client::new();
        let resp = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", &code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", REDIRECT_URI_OOB),
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
        let refresh =
            token
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
            .ok_or_else(|| anyhow::anyhow!("Missing projectId"))?
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
            anyhow::bail!("Refresh failed: {}", body);
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
        serde_json::json!({ "token": credentials.access, "projectId": project_id }).to_string()
    }
}

async fn discover_project(
    access_token: &str,
    callbacks: &dyn OAuthCallbacks,
) -> anyhow::Result<String> {
    if let Ok(project) = std::env::var("GOOGLE_CLOUD_PROJECT") {
        return Ok(project);
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1internal:loadCodeAssist", CODE_ASSIST_ENDPOINT))
        .header("Authorization", format!("Bearer {}", access_token))
        .json(&serde_json::json!({
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI"
            }
        }))
        .send()
        .await?;

    if resp.status().is_success() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LoadResp {
            cloudaicompanion_project: Option<serde_json::Value>,
        }
        if let Ok(data) = resp.json::<LoadResp>().await {
            if let Some(p) = data.cloudaicompanion_project {
                if let Some(s) = p.as_str() {
                    return Ok(s.to_string());
                }
                if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
                    return Ok(id.to_string());
                }
            }
        }
    }

    callbacks.on_progress("Provisioning project...");
    let onboard_resp = client
        .post(format!("{}/v1internal:onboardUser", CODE_ASSIST_ENDPOINT))
        .header("Authorization", format!("Bearer {}", access_token))
        .json(&serde_json::json!({
            "tierId": "free-tier",
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI"
            }
        }))
        .send()
        .await?;

    #[derive(Deserialize)]
    struct OnboardResp {
        response: Option<OnboardResponse>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OnboardResponse {
        cloudaicompanion_project: Option<ProjectId>,
    }
    #[derive(Deserialize)]
    struct ProjectId {
        id: Option<String>,
    }

    if let Ok(data) = onboard_resp.json::<OnboardResp>().await {
        if let Some(r) = data.response {
            if let Some(p) = r.cloudaicompanion_project {
                if let Some(id) = p.id {
                    return Ok(id);
                }
            }
        }
    }
    anyhow::bail!("Project discovery failed")
}
