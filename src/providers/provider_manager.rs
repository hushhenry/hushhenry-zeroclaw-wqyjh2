//! Provider manager: single entry point for obtaining a provider by full model name and temperature.
//! Hides api_key, api_url, and provider/model parsing from callers. Externally, "model" is always
//! the fully-qualified name (e.g. `openrouter/anthropic/claude-sonnet-4` or `group/<group_name>`).
//! Caches providers so repeated get() for the same model returns the same instance.
//!
//! `get()` and `default_resolved()` return a [ProviderCtx]: one object carrying provider,
//! model (for API), and temperature so callers never hold provider and model separately.

use super::create_resilient_provider;
use super::group::{GroupProvider, GroupStrategy};
use super::traits::{ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock;

/// A provider bound to a specific model and temperature. Use this for all chat/compaction calls
/// so provider and model stay in sync.
#[derive(Clone)]
pub struct ProviderCtx {
    pub provider: Arc<dyn Provider>,
    /// Model string to pass to `provider.chat()` (non-qualified, e.g. `anthropic/claude-sonnet-4`).
    pub model: String,
    pub temperature: f64,
}

/// Abstraction for obtaining a resolved provider (allows test doubles).
pub trait ProviderManagerTrait: Send + Sync {
    /// Resolve by full model name and temperature. Returns a single object (provider + model + temp).
    fn get(&self, full_model: &str, temperature: f64) -> anyhow::Result<ProviderCtx>;
    /// Default resolved provider (used when get() fails or for compaction/prompt).
    fn default_resolved(&self) -> anyhow::Result<ProviderCtx>;
    /// Default full model name for display (e.g. `openrouter/anthropic/claude-sonnet-4`).
    fn default_full_model(&self) -> &str;
}

/// Parses a fully-qualified model string "provider/model" into (provider_name, model_for_api).
/// Returns an error if the string is empty or does not contain '/'.
fn parse_full_model(full: &str) -> anyhow::Result<(String, String)> {
    let s = full.trim();
    let i = s
        .find('/')
        .ok_or_else(|| anyhow::anyhow!("Model must be provider/model, got: {}", full))?;
    let provider = s[..i].trim().to_string();
    let model = s[i + 1..].trim().to_string();
    if provider.is_empty() || model.is_empty() {
        anyhow::bail!("Provider and model must be non-empty, got: {}", full);
    }
    Ok((provider, model))
}

/// Wraps a provider and binds the model passed to chat() so the backend receives the model part only.
struct ModelBoundProvider {
    inner: Box<dyn Provider + Send + Sync>,
    model_for_api: String,
}

#[async_trait]
impl Provider for ModelBoundProvider {
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.inner
            .chat(request, &self.model_for_api, temperature)
            .await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        self.inner.warmup().await
    }
}

/// Global provider manager. Build from config once; call `get(full_model, temperature)` to obtain
/// a provider (cached). Supports single providers and `group/<name>` for provider groups.
pub struct ProviderManager {
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
    provider_groups: Vec<crate::config::ProviderGroupConfig>,
    default_full_model: String,
    default_temperature: f64,
    cache: RwLock<HashMap<String, Arc<dyn Provider>>>,
}

impl ProviderManager {
    /// Build one provider for a fully-qualified model (parse + create_resilient_provider + ModelBoundProvider).
    fn create_provider_for_full_model(
        full_model: &str,
        api_key: Option<&str>,
        api_url: Option<&str>,
        reliability: &crate::config::ReliabilityConfig,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let (provider_name, model_for_api) = parse_full_model(full_model.trim())?;
        let box_provider =
            create_resilient_provider(&provider_name, api_key, api_url, reliability)?;
        Ok(Arc::new(ModelBoundProvider {
            inner: box_provider,
            model_for_api,
        }))
    }

    /// Build the manager from config. Creates and caches the default provider.
    pub fn new(config: &crate::config::Config) -> anyhow::Result<Self> {
        let default_provider_name = config.default_provider.as_deref().unwrap_or("openrouter");
        let default_model = config
            .default_model
            .as_deref()
            .unwrap_or("anthropic/claude-sonnet-4");
        let default_full_model = format!("{}/{}", default_provider_name, default_model);
        let default_temperature = config.default_temperature;

        let default_provider = Self::create_provider_for_full_model(
            &default_full_model,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "Config default_provider/default_model must be provider/model: {}",
                e
            )
        })?;

        let cache = RwLock::new(HashMap::from_iter([(
            default_full_model.clone(),
            Arc::clone(&default_provider),
        )]));

        Ok(Self {
            api_key: config.api_key.clone(),
            api_url: config.api_url.clone(),
            reliability: config.reliability.clone(),
            provider_groups: config.provider_groups.clone(),
            default_full_model,
            default_temperature,
            cache,
        })
    }

    /// Default provider (cached at construction). Use this instead of get(default_full_model, …).
    pub fn default_provider(&self) -> Arc<dyn Provider> {
        self.cache
            .read()
            .unwrap()
            .get(&self.default_full_model)
            .cloned()
            .expect("default provider always in cache after new()")
    }

    /// Default full model name (e.g. `openrouter/anthropic/claude-sonnet-4`).
    pub fn default_full_model(&self) -> &str {
        &self.default_full_model
    }

    /// Default temperature from config.
    pub fn default_temperature(&self) -> f64 {
        self.default_temperature
    }

    /// Get or create a provider; uses cache and detects cycles when resolving groups.
    fn get_inner(
        &self,
        full: &str,
        temperature: f64,
        building: &mut HashSet<String>,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        if building.contains(full) {
            anyhow::bail!(
                "Cycle in provider_groups: {} is referenced while being built",
                full
            );
        }
        if let Ok(guard) = self.cache.read() {
            if let Some(p) = guard.get(full) {
                return Ok(Arc::clone(p));
            }
        }
        building.insert(full.to_string());
        let (provider_name, model_part) = parse_full_model(full)?;
        let provider = if provider_name == "group" {
            let group_config = self
                .provider_groups
                .iter()
                .find(|g| g.name == model_part)
                .ok_or_else(|| anyhow::anyhow!("Unknown provider group: {}", model_part))?;
            let members: Vec<Arc<dyn Provider>> = group_config
                .members
                .iter()
                .map(|m| self.get_inner(m.trim(), temperature, building))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Arc::new(GroupProvider::new(
                members,
                GroupStrategy::from(group_config.strategy),
            )) as Arc<dyn Provider>
        } else {
            Self::create_provider_for_full_model(
                full,
                self.api_key.as_deref(),
                self.api_url.as_deref(),
                &self.reliability,
            )?
        };
        building.remove(full);
        self.cache
            .write()
            .unwrap()
            .insert(full.to_string(), Arc::clone(&provider));
        Ok(provider)
    }
}

impl ProviderManagerTrait for ProviderManager {
    fn get(&self, full_model: &str, temperature: f64) -> anyhow::Result<ProviderCtx> {
        let full = full_model.trim();
        let (_, model_for_api) = parse_full_model(full)?;
        let provider = self.get_inner(full, temperature, &mut HashSet::new())?;
        Ok(ProviderCtx {
            provider,
            model: model_for_api,
            temperature,
        })
    }

    fn default_resolved(&self) -> anyhow::Result<ProviderCtx> {
        let provider = self.default_provider();
        let (_, model_for_api) = parse_full_model(self.default_full_model())?;
        Ok(ProviderCtx {
            provider,
            model: model_for_api,
            temperature: self.default_temperature(),
        })
    }

    fn default_full_model(&self) -> &str {
        self.default_full_model()
    }
}
