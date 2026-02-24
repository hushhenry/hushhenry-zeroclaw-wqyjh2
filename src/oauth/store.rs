//! Persist and load OAuth credentials under .zeroclaw (oauth_credentials.json).

use crate::oauth::OAuthCredentials;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

const OAUTH_CREDENTIALS_FILENAME: &str = "oauth_credentials.json";

fn oauth_credentials_path(config_path: &Path) -> Result<std::path::PathBuf> {
    let parent = config_path
        .parent()
        .context("config path has no parent")?;
    Ok(parent.join(OAUTH_CREDENTIALS_FILENAME))
}

/// Load all stored OAuth credentials (provider_id -> credentials).
pub fn load_oauth_credentials(config_path: &Path) -> Result<HashMap<String, OAuthCredentials>> {
    let path = oauth_credentials_path(config_path)?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let map: HashMap<String, OAuthCredentials> =
        serde_json::from_str(&data).context("parse oauth_credentials.json")?;
    Ok(map)
}

/// Save one provider's OAuth credentials (merge with existing).
pub fn save_oauth_credentials(
    config_path: &Path,
    provider_id: &str,
    credentials: &OAuthCredentials,
) -> Result<()> {
    let path = oauth_credentials_path(config_path)?;
    let mut map = load_oauth_credentials(config_path).unwrap_or_default();
    map.insert(provider_id.to_string(), credentials.clone());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(&map).context("serialize oauth credentials")?;
    std::fs::write(&path, data).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Return the current API key string for a provider if we have valid OAuth credentials.
pub fn oauth_api_key_for_provider(
    config_path: &Path,
    provider_id: &str,
) -> Result<Option<String>> {
    let map = load_oauth_credentials(config_path)?;
    let creds = match map.get(provider_id) {
        Some(c) => c,
        None => return Ok(None),
    };
    if chrono::Utc::now().timestamp_millis() >= creds.expires {
        return Ok(None);
    }
    let provider = crate::oauth::oauth_provider_for_id(provider_id);
    let provider = match provider {
        Some(p) => p,
        None => return Ok(None),
    };
    Ok(Some(provider.get_api_key(creds)))
}
