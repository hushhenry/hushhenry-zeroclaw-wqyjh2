pub mod browser;
pub mod browser_open;
pub mod composio;
pub mod cron_add;
pub mod cron_list;
pub mod cron_remove;
pub mod cron_run;
pub mod cron_runs;
pub mod cron_update;
pub mod file_read;
pub mod file_write;
pub mod git_operations;
pub mod http_request;
pub mod image_info;
pub mod memory_forget;
pub mod memory_recall;
pub mod memory_store;
pub mod pushover;
pub mod schedule;
pub mod schema;
pub mod screenshot;
pub mod sessions_history;
pub mod sessions_list;
pub mod sessions_send;
pub mod shell;
mod shell_exec_runtime;
pub mod subagent_poll;
pub mod subagent_send;
pub mod subagent_spawn_oneshot;
pub mod subagent_stop;
pub mod traits;

pub use browser::{BrowserTool, ComputerUseConfig};
pub use browser_open::BrowserOpenTool;
pub use composio::ComposioTool;
pub use cron_add::CronAddTool;
pub use cron_list::CronListTool;
pub use cron_remove::CronRemoveTool;
pub use cron_run::CronRunTool;
pub use cron_runs::CronRunsTool;
pub use cron_update::CronUpdateTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use git_operations::GitOperationsTool;
pub use http_request::HttpRequestTool;
pub use image_info::ImageInfoTool;
pub use memory_forget::MemoryForgetTool;
pub use memory_recall::MemoryRecallTool;
pub use memory_store::MemoryStoreTool;
pub use pushover::PushoverTool;
pub use schedule::ScheduleTool;
#[allow(unused_imports)]
pub use schema::{CleaningStrategy, SchemaCleanr};
pub use screenshot::ScreenshotTool;
pub use sessions_history::SessionsHistoryTool;
pub use sessions_list::SessionsListTool;
pub use sessions_send::SessionsSendTool;
pub use shell::ShellTool;
pub use subagent_poll::SubagentPollTool;
pub use subagent_send::SubagentSendTool;
pub use subagent_spawn_oneshot::SubagentSpawnOneshotTool;
pub use subagent_stop::SubagentStopTool;
pub use traits::Tool;
#[allow(unused_imports)]
pub use traits::{ToolResult, ToolSpec};

use crate::config::{Config, DelegateAgentConfig};
use crate::memory::Memory;
use crate::runtime::{NativeRuntime, RuntimeAdapter};
use crate::security::SecurityPolicy;
use crate::session::SessionStore;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

/// Create the default tool registry
pub fn default_tools(security: Arc<SecurityPolicy>) -> Vec<Box<dyn Tool>> {
    default_tools_with_runtime(security, Arc::new(NativeRuntime::new()))
}

/// Create the default tool registry with explicit runtime adapter.
pub fn default_tools_with_runtime(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ShellTool::new(security.clone(), runtime)),
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security)),
    ]
}

/// Create full tool registry including memory tools and optional Composio
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn all_tools(
    config: Arc<Config>,
    security: &Arc<SecurityPolicy>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &crate::config::BrowserConfig,
    http_config: &crate::config::HttpRequestConfig,
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    root_config: &crate::config::Config,
) -> Vec<Box<dyn Tool>> {
    all_tools_with_runtime(
        config,
        security,
        Arc::new(NativeRuntime::new()),
        memory,
        composio_key,
        composio_entity_id,
        browser_config,
        http_config,
        workspace_dir,
        agents,
        fallback_api_key,
        root_config,
    )
}

/// Create full tool registry including memory tools and optional Composio.
#[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
pub fn all_tools_with_runtime(
    config: Arc<Config>,
    security: &Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    memory: Arc<dyn Memory>,
    composio_key: Option<&str>,
    composio_entity_id: Option<&str>,
    browser_config: &crate::config::BrowserConfig,
    http_config: &crate::config::HttpRequestConfig,
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    root_config: &crate::config::Config,
) -> Vec<Box<dyn Tool>> {
    sync_legacy_delegate_agents_to_subagent_specs(workspace_dir, agents, fallback_api_key);

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ShellTool::new(security.clone(), runtime)),
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(CronAddTool::new(config.clone(), security.clone())),
        Box::new(CronListTool::new(config.clone())),
        Box::new(CronRemoveTool::new(config.clone())),
        Box::new(CronUpdateTool::new(config.clone(), security.clone())),
        Box::new(CronRunTool::new(config.clone())),
        Box::new(CronRunsTool::new(config.clone())),
        Box::new(MemoryStoreTool::new(memory.clone())),
        Box::new(MemoryRecallTool::new(memory.clone())),
        Box::new(MemoryForgetTool::new(memory)),
        Box::new(ScheduleTool::new(security.clone(), root_config.clone())),
        Box::new(GitOperationsTool::new(
            security.clone(),
            workspace_dir.to_path_buf(),
        )),
        Box::new(PushoverTool::new(
            security.clone(),
            workspace_dir.to_path_buf(),
        )),
        Box::new(SessionsListTool::new(workspace_dir.to_path_buf())),
        Box::new(SessionsHistoryTool::new(workspace_dir.to_path_buf())),
        Box::new(SessionsSendTool::new(workspace_dir.to_path_buf())),
        Box::new(SubagentSendTool::new(config.clone())),
        Box::new(SubagentSpawnOneshotTool::new(config.clone())),
        Box::new(SubagentStopTool::new(config)),
        Box::new(SubagentPollTool::new(workspace_dir.to_path_buf())),
    ];

    if browser_config.enabled {
        // Add legacy browser_open tool for simple URL opening
        tools.push(Box::new(BrowserOpenTool::new(
            security.clone(),
            browser_config.allowed_domains.clone(),
        )));
        // Add full browser automation tool (pluggable backend)
        tools.push(Box::new(BrowserTool::new_with_backend(
            security.clone(),
            browser_config.allowed_domains.clone(),
            browser_config.session_name.clone(),
            browser_config.backend.clone(),
            browser_config.native_headless,
            browser_config.native_webdriver_url.clone(),
            browser_config.native_chrome_path.clone(),
            ComputerUseConfig {
                endpoint: browser_config.computer_use.endpoint.clone(),
                api_key: browser_config.computer_use.api_key.clone(),
                timeout_ms: browser_config.computer_use.timeout_ms,
                allow_remote_endpoint: browser_config.computer_use.allow_remote_endpoint,
                window_allowlist: browser_config.computer_use.window_allowlist.clone(),
                max_coordinate_x: browser_config.computer_use.max_coordinate_x,
                max_coordinate_y: browser_config.computer_use.max_coordinate_y,
            },
        )));
    }

    if http_config.enabled {
        tools.push(Box::new(HttpRequestTool::new(
            security.clone(),
            http_config.allowed_domains.clone(),
            http_config.max_response_size,
            http_config.timeout_secs,
        )));
    }

    // Vision tools are always available
    tools.push(Box::new(ScreenshotTool::new(security.clone())));
    tools.push(Box::new(ImageInfoTool::new(security.clone())));

    if let Some(key) = composio_key {
        if !key.is_empty() {
            tools.push(Box::new(ComposioTool::new(key, composio_entity_id)));
        }
    }

    tools
}

fn sync_legacy_delegate_agents_to_subagent_specs(
    workspace_dir: &std::path::Path,
    agents: &HashMap<String, DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
) {
    if agents.is_empty() {
        return;
    }

    tracing::warn!(
        "config.agents is deprecated and the delegate tool has been removed; mapping {} legacy agent configs into subagent specs for compatibility",
        agents.len()
    );

    let store = match SessionStore::new(workspace_dir) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to initialize SessionStore for legacy config.agents migration"
            );
            return;
        }
    };

    let fallback_api_key = fallback_api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    for (name, legacy_agent) in agents {
        let api_key = legacy_agent
            .api_key
            .clone()
            .or_else(|| fallback_api_key.clone());
        let config_json = json!({
            "provider": legacy_agent.provider.clone(),
            "model": legacy_agent.model.clone(),
            "system_prompt": legacy_agent.system_prompt.clone(),
            "api_key": api_key,
            "temperature": legacy_agent.temperature
        });
        let serialized = match serde_json::to_string(&config_json) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    spec_name = name,
                    error = %error,
                    "failed to serialize legacy config.agents entry as subagent spec"
                );
                continue;
            }
        };

        if let Err(error) = store.upsert_subagent_spec(name, serialized.as_str()) {
            tracing::warn!(
                spec_name = name,
                error = %error,
                "failed to upsert legacy config.agents entry as subagent spec"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BrowserConfig, Config, MemoryConfig};
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        }
    }

    #[test]
    fn default_tools_has_three() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn all_tools_excludes_browser_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: false,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"browser_open"));
        assert!(names.contains(&"schedule"));
        assert!(names.contains(&"pushover"));
        assert!(names.contains(&"sessions_list"));
        assert!(names.contains(&"sessions_history"));
        assert!(names.contains(&"sessions_send"));
        assert!(names.contains(&"subagent_send"));
        assert!(names.contains(&"subagent_spawn_oneshot"));
        assert!(names.contains(&"subagent_stop"));
        assert!(names.contains(&"subagent_poll"));
    }

    #[test]
    fn all_tools_includes_browser_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig {
            enabled: true,
            allowed_domains: vec!["example.com".into()],
            session_name: None,
            ..BrowserConfig::default()
        };
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"browser_open"));
        assert!(names.contains(&"pushover"));
        assert!(names.contains(&"sessions_list"));
        assert!(names.contains(&"sessions_history"));
        assert!(names.contains(&"sessions_send"));
        assert!(names.contains(&"subagent_send"));
        assert!(names.contains(&"subagent_spawn_oneshot"));
        assert!(names.contains(&"subagent_stop"));
        assert!(names.contains(&"subagent_poll"));
    }

    #[test]
    fn default_tools_names() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
    }

    #[test]
    fn default_tools_all_have_descriptions() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            assert!(
                !tool.description().is_empty(),
                "Tool {} has empty description",
                tool.name()
            );
        }
    }

    #[test]
    fn default_tools_all_have_schemas() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let schema = tool.parameters_schema();
            assert!(
                schema.is_object(),
                "Tool {} schema is not an object",
                tool.name()
            );
            assert!(
                schema["properties"].is_object(),
                "Tool {} schema has no properties",
                tool.name()
            );
        }
    }

    #[test]
    fn tool_spec_generation() {
        let security = Arc::new(SecurityPolicy::default());
        let tools = default_tools(security);
        for tool in &tools {
            let spec = tool.spec();
            assert_eq!(spec.name, tool.name());
            assert_eq!(spec.description, tool.description());
            assert!(spec.parameters.is_object());
        }
    }

    #[test]
    fn tool_result_serde() {
        let result = ToolResult {
            success: true,
            output: "hello".into(),
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.output, "hello");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn tool_result_with_error_serde() {
        let result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("boom".into()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn tool_spec_serde() {
        let spec = ToolSpec {
            name: "test".into(),
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.description, "A test tool");
    }

    #[test]
    fn all_tools_maps_legacy_agents_to_subagent_specs() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 3,
            },
        );

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            cfg.workspace_dir.as_path(),
            &agents,
            Some("delegate-test-credential"),
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"subagent_spawn_oneshot"));
        assert!(!names.contains(&"delegate"));

        let store = SessionStore::new(cfg.workspace_dir.as_path()).unwrap();
        let spec = store
            .get_subagent_spec_by_name("researcher")
            .unwrap()
            .unwrap();
        assert!(spec.config_json.contains("\"provider\":\"ollama\""));
        assert!(spec
            .config_json
            .contains("\"api_key\":\"delegate-test-credential\""));
    }

    #[test]
    fn all_tools_excludes_delegate_when_no_agents() {
        let tmp = TempDir::new().unwrap();
        let security = Arc::new(SecurityPolicy::default());
        let mem_cfg = MemoryConfig {
            backend: "markdown".into(),
            ..MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> =
            Arc::from(crate::memory::create_memory(&mem_cfg, tmp.path(), None).unwrap());

        let browser = BrowserConfig::default();
        let http = crate::config::HttpRequestConfig::default();
        let cfg = test_config(&tmp);

        let tools = all_tools(
            Arc::new(Config::default()),
            &security,
            mem,
            None,
            None,
            &browser,
            &http,
            tmp.path(),
            &HashMap::new(),
            None,
            &cfg,
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"delegate"));
        assert!(names.contains(&"subagent_spawn_oneshot"));
    }
}
