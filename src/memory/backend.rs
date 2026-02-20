#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MemoryBackendKind {
    Sqlite,
    None,
    Unknown,
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

const NONE_PROFILE: MemoryBackendProfile = MemoryBackendProfile {
    key: "none",
    label: "None — disable persistent memory",
    auto_save_default: false,
    uses_sqlite_hygiene: false,
    sqlite_based: false,
    optional_dependency: false,
};

const SELECTABLE_MEMORY_BACKENDS: [MemoryBackendProfile; 2] = [SQLITE_PROFILE, NONE_PROFILE];

pub fn selectable_memory_backends() -> &'static [MemoryBackendProfile] {
    &SELECTABLE_MEMORY_BACKENDS
}

pub fn default_memory_backend_key() -> &'static str {
    SQLITE_PROFILE.key
}

pub fn classify_memory_backend(backend: &str) -> MemoryBackendKind {
    match backend {
        "sqlite" => MemoryBackendKind::Sqlite,
        "none" => MemoryBackendKind::None,
        _ => MemoryBackendKind::Unknown,
    }
}

pub fn memory_backend_profile(backend: &str) -> MemoryBackendProfile {
    match classify_memory_backend(backend) {
        MemoryBackendKind::Sqlite => SQLITE_PROFILE,
        MemoryBackendKind::None => NONE_PROFILE,
        MemoryBackendKind::Unknown => SQLITE_PROFILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sqlite_backend() {
        assert_eq!(classify_memory_backend("sqlite"), MemoryBackendKind::Sqlite);
    }

    #[test]
    fn classify_none_backend() {
        assert_eq!(classify_memory_backend("none"), MemoryBackendKind::None);
    }

    #[test]
    fn classify_unknown_backend() {
        assert_eq!(classify_memory_backend("redis"), MemoryBackendKind::Unknown);
    }

    #[test]
    fn selectable_backends_sqlite_and_none() {
        let backends = selectable_memory_backends();
        assert_eq!(backends.len(), 2);
        assert_eq!(backends[0].key, "sqlite");
        assert_eq!(backends[1].key, "none");
    }

    #[test]
    fn sqlite_profile_uses_hygiene() {
        let profile = memory_backend_profile("sqlite");
        assert!(profile.uses_sqlite_hygiene);
        assert!(profile.sqlite_based);
    }

    #[test]
    fn none_profile_disables_hygiene_and_auto_save() {
        let profile = memory_backend_profile("none");
        assert_eq!(profile.key, "none");
        assert!(!profile.auto_save_default);
        assert!(!profile.uses_sqlite_hygiene);
    }

    #[test]
    fn unknown_profile_falls_back_to_sqlite() {
        let profile = memory_backend_profile("custom");
        assert_eq!(profile.key, "sqlite");
    }
}
