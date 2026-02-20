pub mod backend;
pub mod chunker;
pub mod embeddings;
pub mod hygiene;
pub mod none;
pub mod response_cache;
pub mod snapshot;
pub mod sqlite;
pub mod traits;
pub mod vector;

#[allow(unused_imports)]
pub use backend::{
    classify_memory_backend, default_memory_backend_key, memory_backend_profile,
    selectable_memory_backends, MemoryBackendKind, MemoryBackendProfile,
};
pub use none::NoneMemory;
pub use response_cache::ResponseCache;
pub use sqlite::SqliteMemory;
pub use traits::Memory;
#[allow(unused_imports)]
pub use traits::{MemoryCategory, MemoryEntry};

use crate::config::MemoryConfig;
use std::path::Path;
use std::sync::Arc;

/// Factory: create the right memory backend from config (sqlite or none).
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    match classify_memory_backend(&config.backend) {
        backend::MemoryBackendKind::None => return Ok(Box::new(NoneMemory::new())),
        backend::MemoryBackendKind::Unknown => {
            tracing::warn!(
                "Unknown memory backend '{}', using sqlite",
                config.backend
            );
        }
        backend::MemoryBackendKind::Sqlite => {}
    }

    // Best-effort memory hygiene/retention pass (throttled by state file).
    if let Err(e) = hygiene::run_if_due(config, workspace_dir) {
        tracing::warn!("memory hygiene skipped: {e}");
    }

    // If snapshot_on_hygiene is enabled, export core memories during hygiene.
    if config.snapshot_enabled && config.snapshot_on_hygiene {
        if let Err(e) = snapshot::export_snapshot(workspace_dir) {
            tracing::warn!("memory snapshot skipped: {e}");
        }
    }

    // Auto-hydration: if brain.db is missing but MEMORY_SNAPSHOT.md exists,
    // restore the "soul" from the snapshot before creating the backend.
    if config.auto_hydrate && snapshot::should_hydrate(workspace_dir) {
        tracing::info!("🧬 Cold boot detected — hydrating from MEMORY_SNAPSHOT.md");
        match snapshot::hydrate_from_snapshot(workspace_dir) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("🧬 Hydrated {count} core memories from snapshot");
                }
            }
            Err(e) => {
                tracing::warn!("memory hydration failed: {e}");
            }
        }
    }

    let embedder: Arc<dyn embeddings::EmbeddingProvider> =
        Arc::from(embeddings::create_embedding_provider(
            &config.embedding_provider,
            api_key,
            &config.embedding_model,
            config.embedding_dimensions,
        ));

    #[allow(clippy::cast_possible_truncation)]
    let mem = SqliteMemory::with_embedder(
        workspace_dir,
        embedder,
        config.vector_weight as f32,
        config.keyword_weight as f32,
        config.embedding_cache_size,
    )?;
    Ok(Box::new(mem))
}

/// Factory: create memory for migration (sqlite only; "none" is rejected).
pub fn create_memory_for_migration(
    backend: &str,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    if classify_memory_backend(backend) == backend::MemoryBackendKind::None {
        anyhow::bail!(
            "memory backend 'none' disables persistence; use sqlite before migration"
        );
    }
    Ok(Box::new(SqliteMemory::new(workspace_dir)?))
}

/// Factory: create an optional response cache from config.
pub fn create_response_cache(config: &MemoryConfig, workspace_dir: &Path) -> Option<ResponseCache> {
    if !config.response_cache_enabled {
        return None;
    }

    match ResponseCache::new(
        workspace_dir,
        config.response_cache_ttl_minutes,
        config.response_cache_max_entries,
    ) {
        Ok(cache) => {
            tracing::info!(
                "💾 Response cache enabled (TTL: {}min, max: {} entries)",
                config.response_cache_ttl_minutes,
                config.response_cache_max_entries
            );
            Some(cache)
        }
        Err(e) => {
            tracing::warn!("Response cache disabled due to error: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn factory_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn factory_none_uses_noop_memory() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "none".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "none");
    }

    #[test]
    fn factory_unknown_falls_back_to_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn migration_factory_returns_sqlite() {
        let tmp = TempDir::new().unwrap();
        let mem = create_memory_for_migration("sqlite", tmp.path()).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn migration_factory_none_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let error = create_memory_for_migration("none", tmp.path())
            .err()
            .expect("backend=none should be rejected for migration");
        assert!(error.to_string().contains("disables persistence"));
    }
}
