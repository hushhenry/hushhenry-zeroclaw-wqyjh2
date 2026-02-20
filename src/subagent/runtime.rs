use crate::config::Config;
use crate::session::SessionStore;
use anyhow::Result;
use parking_lot::Mutex as SyncMutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

pub struct SubagentRuntime {
    pub(crate) store: Arc<SessionStore>,
    pub(crate) config: Arc<Config>,
}

impl SubagentRuntime {
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self::with_config(store, Arc::new(Config::default()))
    }

    pub fn with_config(store: Arc<SessionStore>, config: Arc<Config>) -> Self {
        Self { store, config }
    }

    pub fn shared(config: Arc<Config>) -> Result<Arc<Self>> {
        static RUNTIMES: OnceLock<SyncMutex<HashMap<String, Weak<SubagentRuntime>>>> =
            OnceLock::new();
        let key = config.workspace_dir.to_string_lossy().to_string();
        let runtimes = RUNTIMES.get_or_init(|| SyncMutex::new(HashMap::new()));

        if let Some(existing) = runtimes.lock().get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let store = Arc::new(SessionStore::new(&config.workspace_dir)?);
        let runtime = Arc::new(Self::with_config(store, config));
        runtimes.lock().insert(key, Arc::downgrade(&runtime));
        Ok(runtime)
    }
}

#[cfg(test)]
mod tests {
    use super::SubagentRuntime;
    use crate::session::SessionStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn subagent_runtime_shared_returns_same_runtime_for_same_workspace() {
        let workspace = TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        let config = Arc::new(config);

        let a = SubagentRuntime::shared(Arc::clone(&config)).unwrap();
        let b = SubagentRuntime::shared(Arc::clone(&config)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn subagent_runtime_new_creates_session_via_store() {
        let workspace = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(workspace.path()).unwrap());
        let runtime = SubagentRuntime::new(store);

        let session = runtime.store.create_subagent_session(None, None).unwrap();
        assert!(!session.subagent_session_id.is_empty());
        assert_eq!(session.status, "active");
    }
}
