//! GitHub Copilot OAuth (device flow).

use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthProvider};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// GitHub Copilot OAuth provider (device flow).
pub struct GitHubCopilotOAuthProvider;

#[async_trait]
impl OAuthProvider for GitHubCopilotOAuthProvider {
    fn id(&self) -> &str {
        "github-copilot"
    }
    fn name(&self) -> &str {
        "GitHub Copilot (Device Flow)"
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> anyhow::Result<OAuthCredentials> {
        let client = reqwest::Client::new();

        let resp = client
            .post("https://github.com/login/device/code")
            .header("Accept", "application/json")
            .json(&serde_json::json!({
                "client_id": CLIENT_ID,
                "scope": "read:user"
            }))
            .send()
            .await?;

        #[derive(Deserialize)]
        struct DeviceResp {
            device_code: String,
            user_code: String,
            verification_uri: String,
            interval: u64,
            expires_in: u64,
        }
        let device: DeviceResp = resp.json().await?;

        callbacks.on_auth(OAuthAuthInfo {
            url: device.verification_uri.clone(),
            instructions: Some(format!("Enter code: {}", device.user_code)),
        });

        callbacks.on_progress("Waiting for authorization in browser...");
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(device.interval + 1));
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);

        while std::time::Instant::now() < deadline {
            interval.tick().await;

            let resp = client
                .post("https://github.com/login/oauth/access_token")
                .header("Accept", "application/json")
                .json(&serde_json::json!({
                    "client_id": CLIENT_ID,
                    "device_code": device.device_code,
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
                }))
                .send()
                .await?;

            #[derive(Deserialize)]
            struct TokenResp {
                access_token: Option<String>,
                error: Option<String>,
            }
            let token_resp: TokenResp = resp.json().await?;

            if let Some(access) = token_resp.access_token {
                callbacks.on_progress("Exchanging GitHub token for Copilot token...");
                let copilot_resp = client
                    .get("https://api.github.com/copilot_internal/v2/token")
                    .bearer_auth(&access)
                    .header("User-Agent", "GitHubCopilotChat/0.35.0")
                    .send()
                    .await?;

                #[derive(Deserialize)]
                struct CopilotToken {
                    token: String,
                    expires_at: i64,
                }
                let cp: CopilotToken = copilot_resp.json().await?;

                return Ok(OAuthCredentials {
                    refresh: access,
                    access: cp.token,
                    expires: cp.expires_at * 1000 - 300_000,
                    extra: HashMap::new(),
                });
            }

            if let Some(err) = token_resp.error {
                if err != "authorization_pending" && err != "slow_down" {
                    anyhow::bail!("GitHub error: {}", err);
                }
            }
        }

        anyhow::bail!("Device flow timed out")
    }

    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> anyhow::Result<OAuthCredentials> {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.github.com/copilot_internal/v2/token")
            .bearer_auth(&credentials.refresh)
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .send()
            .await?;

        #[derive(Deserialize)]
        struct CopilotToken {
            token: String,
            expires_at: i64,
        }
        let cp: CopilotToken = resp.json().await?;

        Ok(OAuthCredentials {
            refresh: credentials.refresh.clone(),
            access: cp.token,
            expires: cp.expires_at * 1000 - 300_000,
            extra: HashMap::new(),
        })
    }

    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        credentials.access.clone()
    }
}
