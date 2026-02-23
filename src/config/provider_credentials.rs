//! Provider credentials stored in a separate file (e.g. `providers.json`) so that
//! `config.toml` does not hold API keys or OAuth tokens. Aligns with ZeroAI-style
//! multi-provider configuration.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PROVIDERS_FILENAME: &str = "providers.json";

/// Per-provider credential value. Can be a plain API key (string) or a structured
/// OAuth payload (e.g. for gemini-cli: token + project_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProviderCredentialValue {
    ApiKey(String),
    GeminiCli {
        token: String,
        #[serde(rename = "project_id")]
        project_id: String,
    },
}

impl ProviderCredentialValue {
    /// Resolve to a single string for providers that expect an API key / token.
    /// For Gemini CLI the credential is JSON-serialized so the provider can parse it.
    pub fn as_resolved_string(&self) -> String {
        match self {
            ProviderCredentialValue::ApiKey(s) => s.clone(),
            ProviderCredentialValue::GeminiCli { token, project_id } => {
                serde_json::json!({ "token": token, "projectId": project_id }).to_string()
            }
        }
    }
}

/// Root structure of the providers credentials file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCredentialsFile {
    #[serde(default)]
    pub providers: HashMap<String, ProviderCredentialValue>,
}

/// In-memory store of provider credentials loaded from disk.
#[derive(Debug, Clone, Default)]
pub struct ProviderCredentialsStore {
    path: PathBuf,
    data: ProviderCredentialsFile,
}

impl ProviderCredentialsStore {
    /// Path to the default providers file given the directory containing config.toml.
    pub fn default_path(config_dir: &Path) -> PathBuf {
        config_dir.join(PROVIDERS_FILENAME)
    }

    /// Load from the given path. If the file does not exist, returns an empty store.
    pub fn load(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self {
                path,
                data: ProviderCredentialsFile::default(),
            });
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read provider credentials: {}", path.display()))?;
        let data: ProviderCredentialsFile = serde_json::from_str(&contents).with_context(|| {
            format!(
                "Failed to parse provider credentials JSON: {}",
                path.display()
            )
        })?;
        Ok(Self { path, data })
    }

    /// Save the current store to the same path it was loaded from.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
        let contents = serde_json::to_string_pretty(&self.data)
            .context("Failed to serialize provider credentials")?;
        std::fs::write(&self.path, contents).with_context(|| {
            format!(
                "Failed to write provider credentials: {}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    /// Get resolved credential string for a provider id, if present.
    pub fn get(&self, provider_id: &str) -> Option<String> {
        self.data
            .providers
            .get(provider_id)
            .map(|v| v.as_resolved_string())
    }

    /// Set credential for a provider (API key string).
    pub fn set_api_key(&mut self, provider_id: &str, api_key: String) {
        self.data.providers.insert(
            provider_id.to_string(),
            ProviderCredentialValue::ApiKey(api_key),
        );
    }

    /// Set Gemini CLI (OAuth) credential.
    pub fn set_gemini_cli(&mut self, token: String, project_id: String) {
        self.data.providers.insert(
            "gemini-cli".to_string(),
            ProviderCredentialValue::GeminiCli { token, project_id },
        );
    }

    /// Path this store was loaded from / will save to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All provider ids that have credentials.
    pub fn provider_ids(&self) -> impl Iterator<Item = &String> {
        self.data.providers.keys()
    }
}
