/// Memory backend selection.
///
/// Milestone 0 decision: **sqlite-only**.
///
/// We keep the backend module as a tiny compatibility surface so config parsing
/// does not need to change dramatically, but the runtime will only construct the
/// SQLite-backed memory.

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MemoryBackendKind {
    Sqlite,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MemoryBackendProfile {
    pub key: &'static str,
    pub label: &'static str,
    pub auto_save_default: bool,
    pub uses_sqlite_hygiene: bool,
    pub sqlite_based: bool,
    pub optional_dependency: bool,
}

const SQLITE_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "sqlite",
    label: "SQLite with Vector Search — fast, hybrid search, embeddings",
    auto_save_default: true,
    uses_sqlite_hygiene: true,
    sqlite_based: true,
    optional_dependency: false,
};

const SELECTABLE_MEMORY_BACKENDS: [MemoryBackendProfile; 1] = [SQLITE_PROFILE];

pub fn selectable_memory_backends() -> &'static [MemoryBackendProfile] {
    &SELECTABLE_MEMORY_BACKENDS
}

pub fn default_memory_backend_key() -> &'static str {
    SQLITE_PROFILE.key
}

pub fn classify_memory_backend(_backend: &str) -> MemoryBackendKind {
    // sqlite-only: ignore the configured backend and always use sqlite.
    MemoryBackendKind::Sqlite
}

pub fn memory_backend_profile(_backend: &str) -> MemoryBackendProfile {
    SQLITE_PROFILE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_only() {
        assert_eq!(classify_memory_backend("sqlite"), MemoryBackendKind::Sqlite);
        assert_eq!(
            classify_memory_backend("markdown"),
            MemoryBackendKind::Sqlite
        );
        assert_eq!(selectable_memory_backends().len(), 1);
        assert_eq!(selectable_memory_backends()[0].key, "sqlite");
        assert_eq!(memory_backend_profile("anything").key, "sqlite");
    }
}
