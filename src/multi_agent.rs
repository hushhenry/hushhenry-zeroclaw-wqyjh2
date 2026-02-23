//! Multi-agent workspace layout and resolution.
//!
//! - Base workspace is `$CONFIG_DIR/workspace`. PROMPT files (IDENTITY.md, USER.md, AGENTS.md, etc.) live under `workspace/<agent_id>/`.
//! - Main agent always uses `workspace/main/`; other agents use `workspace/<agent_id>/`.
//! - When default agent is set (e.g. `worker`): that agent also gets read/write on base (root); main stays `workspace/main/`.

use std::path::{Path, PathBuf};

/// Default agent id when no override is set (has access to entire base workspace).
pub const DEFAULT_AGENT_ID: &str = "main";

const DEFAULT_AGENT_ID_FILE: &str = "default_agent_id";

/// Read the current default agent id from config dir (file `default_agent_id`). None if unset or main.
pub fn get_default_agent_id(config_dir: &Path) -> Option<String> {
    let path = config_dir.join(DEFAULT_AGENT_ID_FILE);
    let s = std::fs::read_to_string(&path).ok()?;
    let id = s.trim();
    if id.is_empty() || id == DEFAULT_AGENT_ID {
        None
    } else {
        Some(id.to_string())
    }
}

/// Write the default agent id to config dir. Use "main" or empty to clear (main regains root).
pub fn set_default_agent_id(config_dir: &Path, agent_id: &str) -> anyhow::Result<()> {
    let path = config_dir.join(DEFAULT_AGENT_ID_FILE);
    let id = agent_id.trim();
    if id.is_empty() || id == DEFAULT_AGENT_ID {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::write(&path, id)?;
    Ok(())
}

/// Resolve the effective workspace directory for an agent (where PROMPT files like IDENTITY.md, USER.md, AGENTS.md live).
///
/// - Main always → base/main. Others → base/agent_id.
/// - When `default_agent_id` is set (e.g. "worker"): that agent also has access to base (root) for cross-workspace use; main stays base/main.
pub fn workspace_dir_for_agent(
    base_workspace: &Path,
    agent_id: &str,
    default_agent_id: Option<&str>,
) -> PathBuf {
    let id = agent_id.trim();
    let default_set = default_agent_id
        .map(|d| !d.is_empty() && d != DEFAULT_AGENT_ID)
        .unwrap_or(false);
    if id.is_empty() || id == DEFAULT_AGENT_ID {
        base_workspace.join(DEFAULT_AGENT_ID)
    } else if default_set && default_agent_id == Some(id) {
        base_workspace.to_path_buf()
    } else {
        base_workspace.join(id)
    }
}

/// Validate agent_id for creation: only letters (a–z, A–Z), underscore, hyphen; not "main".
pub fn is_valid_new_agent_id(agent_id: &str) -> bool {
    let s = agent_id.trim();
    if s.is_empty() || s == DEFAULT_AGENT_ID {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_dir_main_is_base_main() {
        let base = Path::new("/tmp/ws");
        assert_eq!(
            workspace_dir_for_agent(base, "main", None),
            PathBuf::from("/tmp/ws/main")
        );
        assert_eq!(
            workspace_dir_for_agent(base, "", None),
            PathBuf::from("/tmp/ws/main")
        );
    }

    #[test]
    fn workspace_dir_other_is_subdir() {
        let base = Path::new("/tmp/ws");
        assert_eq!(
            workspace_dir_for_agent(base, "worker", None),
            PathBuf::from("/tmp/ws/worker")
        );
    }

    #[test]
    fn workspace_dir_when_default_agent_set() {
        let base = Path::new("/tmp/ws");
        assert_eq!(
            workspace_dir_for_agent(base, "main", Some("worker")),
            PathBuf::from("/tmp/ws/main")
        );
        assert_eq!(
            workspace_dir_for_agent(base, "worker", Some("worker")),
            PathBuf::from("/tmp/ws")
        );
        assert_eq!(
            workspace_dir_for_agent(base, "other", Some("worker")),
            PathBuf::from("/tmp/ws/other")
        );
    }

    #[test]
    fn is_valid_new_agent_id_rejects_main_and_empty() {
        assert!(!is_valid_new_agent_id("main"));
        assert!(!is_valid_new_agent_id(""));
        assert!(!is_valid_new_agent_id("  "));
    }

    #[test]
    fn is_valid_new_agent_id_accepts_letters_underscore_hyphen() {
        assert!(is_valid_new_agent_id("worker"));
        assert!(is_valid_new_agent_id("agent_one"));
        assert!(is_valid_new_agent_id("my-agent"));
        assert!(is_valid_new_agent_id("Agent_One"));
    }

    #[test]
    fn is_valid_new_agent_id_rejects_digits() {
        assert!(!is_valid_new_agent_id("agent1"));
        assert!(!is_valid_new_agent_id("agent_1"));
    }
}
