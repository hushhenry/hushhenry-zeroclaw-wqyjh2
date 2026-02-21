pub mod cli;
pub mod dingtalk;
pub mod discord;
pub mod email_channel;
pub mod imessage;
pub mod irc;
pub mod lark;
pub mod matrix;
pub mod mattermost;
pub mod qq;
pub mod signal;
pub mod slack;
pub mod telegram;
pub mod traits;
pub mod whatsapp;

pub use cli::CliChannel;
pub use dingtalk::DingTalkChannel;
pub use discord::DiscordChannel;
pub use email_channel::EmailChannel;
pub use imessage::IMessageChannel;
pub use irc::IrcChannel;
pub use lark::LarkChannel;
pub use matrix::MatrixChannel;
pub use mattermost::MattermostChannel;
pub use qq::QQChannel;
pub use signal::SignalChannel;
pub use slack::SlackChannel;
pub use telegram::TelegramChannel;
pub use traits::{Channel, SendMessage};
pub use whatsapp::WhatsAppChannel;

use crate::agent::command::{build_route_metadata, handle_slash_command, parse_slash_command};
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::agent::turn::{run_memory_turn, run_session_turn};
use crate::config::Config;
use crate::memory::{self, Memory};
use crate::observability::{self, Observer};
use crate::providers::{self, ChatMessage, Provider, ProviderManagerTrait, ProviderCtx};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::session::{
    compaction::{
        build_compaction_summary_message, build_merged_system_prompt, CompactionState,
    },
    SessionContext, SessionId, SessionMessage, SessionMessageRole, SessionResolver,
    SessionRouteMetadata, SessionStore,
};
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::{Context, Result};
use parking_lot::Mutex as ParkingMutex;
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Maximum characters per injected workspace file (matches `OpenClaw` default).
const BOOTSTRAP_MAX_CHARS: usize = crate::agent::prompt::DEFAULT_BOOTSTRAP_MAX_CHARS;

/// Reserved channel for internal/child sessions. Sessions with this channel do not deliver to external users (M4).
pub const INTERNAL_MESSAGE_CHANNEL: &str = "internal";
const INTERNAL_SESSION_ID_PREFIX: &str = "session:";

/// Timeout for a single turn (LLM + tools). Exposed for agent turn execution.
pub(crate) const CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 300;

const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
const CHANNEL_PARALLELISM_PER_CHANNEL: usize = 4;
const CHANNEL_MIN_IN_FLIGHT_MESSAGES: usize = 8;
const CHANNEL_MAX_IN_FLIGHT_MESSAGES: usize = 64;

#[derive(Clone)]
pub(crate) struct ChannelRuntimeContext {
    pub(crate) channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    pub(crate) provider_manager: Arc<dyn ProviderManagerTrait>,
    pub(crate) memory: Arc<dyn Memory>,
    pub(crate) tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub(crate) observer: Arc<dyn Observer>,
    pub(crate) system_prompt: Arc<String>,
    pub(crate) auto_save_memory: bool,
    pub(crate) session_enabled: bool,
    pub(crate) session_history_limit: u32,
    pub(crate) session_store: Option<Arc<SessionStore>>,
    pub(crate) session_resolver: SessionResolver,
    /// Config for per-session agent/model resolution (multi-agent).
    pub(crate) config: Arc<Config>,
    /// All skills (for per-agent filtering in Milestone 2).
    pub(crate) all_skills: Arc<Vec<crate::skills::Skill>>,
}

/// One unit of work for an agent's internal queue. External and internal producers both push this.
#[derive(Clone)]
struct AgentWorkItem {
    content: String,
    reply_target: String,
    channel: String,
    sender: String,
    chat_id: String,
    thread_id: Option<String>,
    agent_id: Option<String>,
    account_id: Option<String>,
    title: Option<String>,
    chat_type: traits::ChatType,
    raw_chat_type: Option<String>,
}

impl AgentWorkItem {
    fn from_message(msg: &traits::ChannelMessage) -> Self {
        Self {
            content: msg.content.clone(),
            reply_target: msg.reply_target.clone(),
            channel: msg.channel.clone(),
            sender: msg.sender.clone(),
            chat_id: msg.chat_id.clone(),
            thread_id: msg.thread_id.clone(),
            agent_id: msg.agent_id.clone(),
            account_id: msg.account_id.clone(),
            title: msg.title.clone(),
            chat_type: msg.chat_type,
            raw_chat_type: msg.raw_chat_type.clone(),
        }
    }

    fn to_channel_message(&self) -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: Uuid::new_v4().to_string(),
            agent_id: self.agent_id.clone(),
            account_id: self.account_id.clone(),
            sender: self.sender.clone(),
            reply_target: self.reply_target.clone(),
            content: self.content.clone(),
            channel: self.channel.clone(),
            title: self.title.clone(),
            chat_type: self.chat_type,
            raw_chat_type: self.raw_chat_type.clone(),
            chat_id: self.chat_id.clone(),
            thread_id: self.thread_id.clone(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

struct AgentHandle {
    tx: mpsc::Sender<AgentWorkItem>,
}

/// Registry of per-session agents. Key = session_id. Unregistered when agent loop exits.
fn agent_registry() -> Arc<ParkingMutex<HashMap<String, AgentHandle>>> {
    static REGISTRY: LazyLock<Arc<ParkingMutex<HashMap<String, AgentHandle>>>> =
        LazyLock::new(|| Arc::new(ParkingMutex::new(HashMap::new())));
    REGISTRY.clone()
}

fn internal_dispatch_sender() -> Arc<ParkingMutex<Option<mpsc::Sender<traits::ChannelMessage>>>> {
    static INTERNAL_DISPATCH: LazyLock<
        Arc<ParkingMutex<Option<mpsc::Sender<traits::ChannelMessage>>>>,
    > = LazyLock::new(|| Arc::new(ParkingMutex::new(None)));
    INTERNAL_DISPATCH.clone()
}

fn set_internal_dispatch_sender(sender: Option<mpsc::Sender<traits::ChannelMessage>>) {
    let registry = internal_dispatch_sender();
    let mut guard = registry.lock();
    *guard = sender;
}

pub async fn dispatch_internal_message(message: traits::ChannelMessage) -> Result<()> {
    let tx_opt = {
        let registry = internal_dispatch_sender();
        let guard = registry.lock();
        guard.clone()
    };
    let tx = tx_opt.ok_or_else(|| anyhow::anyhow!("channel dispatcher is not running"))?;
    tx.send(message)
        .await
        .map_err(|_| anyhow::anyhow!("channel dispatcher queue is closed"))
}

pub fn build_internal_channel_message(
    sender: impl Into<String>,
    target_session_id: impl Into<String>,
    content: impl Into<String>,
    session_key: Option<&str>,
) -> traits::ChannelMessage {
    let chat_id = format!("{INTERNAL_SESSION_ID_PREFIX}{}", target_session_id.into());
    traits::ChannelMessage {
        id: Uuid::new_v4().to_string(),
        agent_id: None,
        account_id: None,
        sender: sender.into(),
        reply_target: chat_id.clone(),
        content: content.into(),
        channel: INTERNAL_MESSAGE_CHANNEL.to_string(),
        title: None,
        chat_type: traits::ChatType::Direct,
        raw_chat_type: Some("internal".to_string()),
        chat_id,
        thread_id: session_key.map(ToOwned::to_owned),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    }
}

pub fn parse_internal_target_session_id(msg: &traits::ChannelMessage) -> Option<SessionId> {
    if msg.channel != INTERNAL_MESSAGE_CHANNEL {
        return None;
    }
    msg.chat_id
        .strip_prefix(INTERNAL_SESSION_ID_PREFIX)
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| SessionId::from_string(raw.to_string()))
}

pub(crate) async fn send_delivery_message(
    target_channel: Option<&Arc<dyn Channel>>,
    delivery_channel_name: &str,
    delivery_reply_target: &str,
    content: &str,
) -> Result<()> {
    if delivery_channel_name == INTERNAL_MESSAGE_CHANNEL {
        let target_session_id = delivery_reply_target
            .strip_prefix(INTERNAL_SESSION_ID_PREFIX)
            .unwrap_or(delivery_reply_target)
            .trim();
        if target_session_id.is_empty() {
            anyhow::bail!("internal delivery target session_id is empty");
        }
        let msg =
            build_internal_channel_message("zeroclaw_internal", target_session_id, content, None);
        return dispatch_internal_message(msg).await;
    }

    let channel = target_channel
        .ok_or_else(|| anyhow::anyhow!("delivery channel not found: {delivery_channel_name}"))?;
    channel
        .send(&SendMessage::new(content, delivery_reply_target))
        .await
}

pub(crate) fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.id)
}

pub(crate) fn decode_session_string_state(value_json: Option<String>) -> Option<String> {
    value_json.and_then(|raw| {
        serde_json::from_str::<String>(&raw).ok().or_else(|| {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
    })
}

/// Minimal parsed shape of AgentSpec.config_json for turn resolution.
#[derive(serde::Deserialize, Default)]
struct AgentSpecDefaults {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
}

/// Allow-list policy for tools and skills (Milestone 2).
#[derive(serde::Deserialize, Default)]
pub(crate) struct AgentSpecPolicy {
    #[serde(default)]
    pub(crate) tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) skills: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct AgentSpecConfigJson {
    #[serde(default)]
    defaults: AgentSpecDefaults,
    #[serde(default)]
    policy: AgentSpecPolicy,
}

fn parse_agent_spec_defaults(config_json: &str) -> AgentSpecDefaults {
    serde_json::from_str::<AgentSpecConfigJson>(config_json)
        .ok()
        .map(|c| c.defaults)
        .unwrap_or_default()
}

fn parse_agent_spec_policy(config_json: &str) -> AgentSpecPolicy {
    serde_json::from_str::<AgentSpecConfigJson>(config_json)
        .ok()
        .map(|c| c.policy)
        .unwrap_or_default()
}

/// Session (provider, model, temperature). Returns Some only when model_override is set and contains '/' (parsed to provider/model).
fn get_session_model_temperature(
    store: &SessionStore,
    session_id: &SessionId,
    config: &Config,
) -> Option<(String, String, f64)> {
    let raw = decode_session_string_state(
        store
            .get_state_key(session_id, SessionStore::MODEL_OVERRIDE_KEY)
            .ok()
            .flatten(),
    )?;
    let s = raw.trim();
    let (provider, model) = s.split_once('/').map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider, model, config.default_temperature))
}

/// Agent (provider, model, temperature) from active AgentSpec defaults. Returns Some only when all three are set.
fn get_agent_model_temperature(
    store: &SessionStore,
    session_id: &SessionId,
) -> Option<(String, String, f64)> {
    let active_agent_id = decode_session_string_state(
        store
            .get_state_key(session_id, SessionStore::ACTIVE_AGENT_ID_KEY)
            .ok()
            .flatten(),
    )?;
    let spec = store
        .get_agent_spec_by_id(&active_agent_id)
        .ok()
        .flatten()
        .or_else(|| store.get_agent_spec_by_name(&active_agent_id).ok().flatten())?;
    let d = parse_agent_spec_defaults(&spec.config_json);
    let p = d.provider?;
    let m = d.model?;
    let t = d.temperature?;
    Some((p, m, t))
}

/// Config default (provider, model, temperature).
fn get_config_model_temperature(config: &Config) -> (String, String, f64) {
    let p = config
        .default_provider
        .as_deref()
        .unwrap_or("openrouter");
    let m = config
        .default_model
        .as_deref()
        .unwrap_or("anthropic/claude-sonnet-4");
    (p.to_string(), m.to_string(), config.default_temperature)
}

/// Resolve provider + model + temperature for one turn: session → agent → config, then manager.get().
/// Returns a single [ProviderCtx] so provider and model stay in sync.
pub(crate) fn resolve_turn_provider_model_temperature(
    ctx: &ChannelRuntimeContext,
    active_session: Option<&SessionId>,
) -> ProviderCtx {
    let (provider_name, model, temp) = match (ctx.session_store.as_deref(), active_session) {
        (Some(store), Some(sid)) => {
            get_session_model_temperature(store, sid, &ctx.config)
                .or_else(|| get_agent_model_temperature(store, sid))
                .unwrap_or_else(|| get_config_model_temperature(&ctx.config))
        }
        _ => get_config_model_temperature(&ctx.config),
    };

    let full_model = format!("{}/{}", provider_name, model);

    match ctx.provider_manager.get(&full_model, temp) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "ProviderManager.get({:?}, {}) failed: {e}; using default",
                full_model,
                temp
            );
            ctx.provider_manager
                .default_resolved()
                .unwrap_or_else(|_| panic!("default_resolved() failed after get() fallback"))
        }
    }
}

/// Resolve AgentSpec policy for a session (tools/skills allow-lists). Returns None if no active agent or no policy.
pub(crate) fn resolve_agent_spec_policy(
    session_store: &SessionStore,
    session_id: &SessionId,
) -> Option<AgentSpecPolicy> {
    let active_agent_id = decode_session_string_state(
        session_store
            .get_state_key(session_id, SessionStore::ACTIVE_AGENT_ID_KEY)
            .ok()
            .flatten(),
    )?;
    let spec = session_store
        .get_agent_spec_by_id(&active_agent_id)
        .ok()
        .flatten()
        .or_else(|| {
            session_store
                .get_agent_spec_by_name(&active_agent_id)
                .ok()
                .flatten()
        })?;
    let policy = parse_agent_spec_policy(&spec.config_json);
    let has_policy = policy.tools.is_some() || policy.skills.is_some();
    if has_policy {
        Some(policy)
    } else {
        None
    }
}

/// Resolve effective system prompt and optional tool allow-list for a session.
/// Shared by the agent turn (history + compaction + tool loop) and manual /compact.
/// Returns (prompt, tool_allow_list); when no session/policy, tool_allow_list is None.
pub(crate) fn resolve_effective_system_prompt_and_tool_allow_list(
    ctx: &ChannelRuntimeContext,
    session_id: Option<&SessionId>,
    channel_name: &str,
) -> (String, Option<Vec<String>>) {
    let default_merged = build_merged_system_prompt(
        ctx.system_prompt.as_str(),
        channel_delivery_instructions(channel_name),
    );
    let (Some(store), Some(sid)) = (ctx.session_store.as_ref(), session_id) else {
        return (default_merged, None);
    };
    let Some(policy) = resolve_agent_spec_policy(store, sid) else {
        return (default_merged, None);
    };
    let allowed_tools = policy.tools.clone();
    let allowed_skills = policy.skills;
    let tool_entries: Vec<(&str, &str)> = ctx
        .tools_registry
        .iter()
        .filter(|tool| match allowed_tools.as_ref() {
            Some(allow) => allow.iter().any(|name| name == tool.name()),
            None => true,
        })
        .map(|tool| (tool.name(), tool.description()))
        .collect();
    let filtered_skills_vec: Vec<crate::skills::Skill> =
        if let Some(ref allow) = allowed_skills {
            ctx.all_skills
                .iter()
                .filter(|skill| allow.iter().any(|name| name == &skill.name))
                .cloned()
                .collect()
        } else {
            ctx.all_skills.to_vec()
        };
    let bootstrap_max_chars = if ctx.config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let base_prompt = build_system_prompt(
        &ctx.config.workspace_dir,
        ctx.provider_manager.default_full_model(),
        &tool_entries,
        &filtered_skills_vec,
        Some(&ctx.config.identity),
        bootstrap_max_chars,
    );
    let merged = build_merged_system_prompt(
        &base_prompt,
        channel_delivery_instructions(channel_name),
    );
    (merged, allowed_tools)
}

fn normalize_session_context(msg: &traits::ChannelMessage) -> SessionContext {
    SessionContext {
        channel: msg.channel.clone(),
        chat_type: msg.chat_type,
        sender_id: msg.sender.clone(),
        chat_id: msg.chat_id.clone(),
        thread_id: msg.thread_id.clone(),
    }
}

pub(crate) fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.",
        ),
        _ => None,
    }
}

pub(crate) async fn build_memory_context(
    mem: &dyn Memory,
    user_msg: &str,
    session_id: Option<&str>,
) -> String {
    let mut context = String::new();

    if let Ok(entries) = mem.recall(user_msg, 5, session_id).await {
        if !entries.is_empty() {
            context.push_str("[Memory context]\n");
            for entry in &entries {
                let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
            }
            context.push('\n');
        }
    }

    context
}

fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(&component);
            let result = ch.listen(tx.clone()).await;

            if tx.is_closed() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    tracing::error!("Channel {} error: {e}; restarting", ch.name());
                    crate::health::mark_component_error(&component, e.to_string());
                }
            }

            crate::health::bump_component_restart(&component);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

fn compute_max_in_flight_messages(channel_count: usize) -> usize {
    channel_count
        .saturating_mul(CHANNEL_PARALLELISM_PER_CHANNEL)
        .clamp(
            CHANNEL_MIN_IN_FLIGHT_MESSAGES,
            CHANNEL_MAX_IN_FLIGHT_MESSAGES,
        )
}

fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

/// Returns false when the session is bound to INTERNAL_MESSAGE_CHANNEL (no external delivery). Used for M4 child sessions.
pub(crate) fn should_deliver_to_external_channel(
    session_store: Option<&Arc<SessionStore>>,
    active_session: Option<&SessionId>,
) -> bool {
    let (Some(store), Some(session_id)) = (session_store, active_session) else {
        return true;
    };
    !matches!(
        store.load_route_metadata(session_id),
        Ok(Some(meta)) if meta.channel == INTERNAL_MESSAGE_CHANNEL
    )
}

/// M4: Build ephemeral context from recent announce messages (meta_json with source/result). Not persisted.
pub(crate) fn build_ephemeral_announce_context(messages: &[SessionMessage]) -> String {
    const MAX_RESULT_PREVIEW: usize = 120;
    let mut lines: Vec<String> = Vec::new();
    for msg in messages {
        if msg.role != "assistant" {
            continue;
        }
        let Some(meta_str) = msg.meta_json.as_deref() else {
            continue;
        };
        let meta: serde_json::Value = match serde_json::from_str(meta_str) {
            Ok(m) => m,
            _ => continue,
        };
        let source = match meta.get("source").and_then(|s| s.as_object()) {
            Some(s) => s,
            _ => continue,
        };
        let agent_id = source
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let result_preview = meta
            .get("result")
            .and_then(|r| r.as_str())
            .map(|s| {
                let t = s.trim();
                if t.len() <= MAX_RESULT_PREVIEW {
                    t.to_string()
                } else {
                    format!("{}…", &t[..MAX_RESULT_PREVIEW])
                }
            })
            .unwrap_or_else(|| "(no result)".to_string());
        lines.push(format!(
            "- @{}: {}",
            agent_id,
            result_preview.replace('\n', " ")
        ));
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("[Subagent results]\n{}", lines.join("\n"))
    }
}

/// Build session turn history: system prompt, optional compaction summary, tail messages as chat, and the given user message.
pub(crate) fn build_session_turn_history(
    system_prompt: &str,
    compaction_state: &CompactionState,
    tail_messages: &[SessionMessage],
    user_content: &str,
) -> Vec<ChatMessage> {
    let mut history = vec![ChatMessage::system(system_prompt)];
    if let Some(summary) = compaction_state.summary.as_deref() {
        history.push(build_compaction_summary_message(summary));
    }
    for message in tail_messages {
        match SessionMessageRole::from_str(message.role.as_str()) {
            Some(SessionMessageRole::User) => {
                history.push(ChatMessage::user(message.content.clone()));
            }
            Some(SessionMessageRole::Assistant) => {
                history.push(ChatMessage::assistant(message.content.clone()));
            }
            Some(SessionMessageRole::Tool) => {
                history.push(ChatMessage::tool(message.content.clone()));
            }
            None => {
                tracing::warn!(
                    role = message.role.as_str(),
                    "Skipping unsupported role from stored session history"
                );
            }
        }
    }
    history.push(ChatMessage::user(user_content));
    history
}

async fn process_channel_message(ctx: Arc<ChannelRuntimeContext>, msg: traits::ChannelMessage) {
    println!(
        "  💬 [{}] from {}: {}",
        msg.channel,
        msg.sender,
        truncate_with_ellipsis(&msg.content, 80)
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    let parsed_command = parse_slash_command(&msg.content);
    let session_context = normalize_session_context(&msg);
    let session_key = ctx.session_resolver.resolve(&session_context);

    if let Some(command) = parsed_command {
        let handled =
            handle_slash_command(&ctx, target_channel.as_ref(), &msg, &session_key, command)
                .await;
        if handled {
            return;
        }
    }

    if ctx.session_enabled {
        let session_id = if let Some(internal_target) = parse_internal_target_session_id(&msg) {
            internal_target
        } else {
            let Some(store) = ctx.session_store.as_ref() else {
                tracing::error!("Session store missing in session mode");
                return;
            };
            match store.get_or_create_active(&session_key) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(session_key = %session_key, "Failed to resolve active session: {e}");
                    return;
                }
            }
        };

        // Enqueue to agent's internal queue only; agent loop consumes and runs the turn.
        let work = AgentWorkItem::from_message(&msg);
        let tx = get_or_create_agent(Arc::clone(&ctx), session_id.clone());
        if tx.send(work).await.is_err() {
            tracing::debug!(session_id = %session_id.as_str(), "Agent queue closed");
        }
        return;
    }

    // Non-session path: run one turn directly (no agent queue).
    let active_session = ctx
        .session_store
        .as_ref()
        .and_then(|store| store.get_or_create_active(&session_key).ok());
    if let Err(e) = run_memory_turn(ctx.clone(), active_session.as_ref(), msg).await {
        tracing::error!("Session turn failed: {e}");
    }
}

/// Returns a sender to the agent's queue. Caller enqueues work; agent loop is the only consumer.
fn get_or_create_agent(
    ctx: Arc<ChannelRuntimeContext>,
    session_id: SessionId,
) -> mpsc::Sender<AgentWorkItem> {
    let key_str = session_id.as_str().to_string();
    let registry = agent_registry();
    let mut guard = registry.lock();
    if let Some(handle) = guard.get(&key_str) {
        return handle.tx.clone();
    }
    let (tx, rx) = mpsc::channel(64);
    let handle = AgentHandle { tx: tx.clone() };
    guard.insert(key_str.clone(), handle);
    drop(guard);
    let runner_ctx = Arc::clone(&ctx);
    tokio::spawn(agent_loop(registry, key_str, session_id, rx, runner_ctx));
    tx
}

/// Drain receiver without blocking.
fn drain_agent_queue(rx: &mut mpsc::Receiver<AgentWorkItem>) -> Vec<AgentWorkItem> {
    let mut batch: Vec<AgentWorkItem> = Vec::new();
    while let Ok(w) = rx.try_recv() {
        batch.push(w);
    }
    batch
}

fn merge_work_items(mut batch: Vec<AgentWorkItem>) -> Option<AgentWorkItem> {
    if batch.is_empty() {
        return None;
    }
    let last = batch.pop()?;
    let mut contents: Vec<String> = batch.into_iter().map(|w| w.content).collect();
    contents.push(last.content.clone());
    Some(AgentWorkItem {
        content: contents.join("\n\n"),
        ..last
    })
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn build_steer_merge_message(current_user_content: &str, pending: Vec<AgentWorkItem>) -> String {
    let mut out = String::from("<messages>\n");
    out.push_str("<item>");
    out.push_str(&escape_xml_text(current_user_content));
    out.push_str("</item>\n");
    for item in pending {
        out.push_str("<item>");
        out.push_str(&escape_xml_text(item.content.as_str()));
        out.push_str("</item>\n");
    }
    out.push_str("</messages>");
    out
}

/// Agent run loop: sole consumer of this agent's queue. Runs one turn per work item and performs
/// checkpoint steer-merge by non-blocking drain.
async fn agent_loop(
    registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
    key_str: String,
    session_id: SessionId,
    mut rx: mpsc::Receiver<AgentWorkItem>,
    ctx: Arc<ChannelRuntimeContext>,
) {
    loop {
        let first = match rx.recv().await {
            Some(w) => w,
            None => break,
        };
        let mut batch = vec![first];
        batch.extend(drain_agent_queue(&mut rx));
        let Some(work) = merge_work_items(batch) else {
            continue;
        };

        let session_store = match ctx.session_store.as_ref() {
            Some(s) => s,
            None => continue,
        };
        let msg = work.to_channel_message();
        if msg.channel != INTERNAL_MESSAGE_CHANNEL {
            if let Err(e) =
                session_store.upsert_route_metadata(&session_id, &build_route_metadata(&msg))
            {
                tracing::warn!(
                    session_id = %session_id.as_str(),
                    "Failed to persist route metadata: {e}"
                );
            }
        }
        let mut steer_fn = |current_user_content: &str| {
            let pending = drain_agent_queue(&mut rx);
            if pending.is_empty() {
                None
            } else {
                Some(build_steer_merge_message(current_user_content, pending))
            }
        };
        if let Err(e) = run_session_turn(ctx.clone(), &session_id, msg, Some(&mut steer_fn)).await {
            tracing::error!(session_id = %session_id.as_str(), "Agent turn error: {e}");
        }
    }
    registry.lock().remove(&key_str);
}

async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();

    while let Some(msg) = rx.recv().await {
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        workers.spawn(async move {
            let _permit = permit;
            process_channel_message(worker_ctx, msg).await;
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default:
/// 1. Tooling — tool list + descriptions
/// 2. Safety — guardrail reminder
/// 3. Skills — compact list with paths (loaded on-demand)
/// 4. Workspace — working directory
/// 5. Bootstrap files — AGENTS, SOUL, TOOLS, IDENTITY, USER, HEARTBEAT, BOOTSTRAP, MEMORY
/// 6. Date & Time — timezone for cache stability
/// 7. Runtime — host, OS, model
///
/// When `identity_config` is set to AIEOS format, the bootstrap files section
/// is replaced with the AIEOS identity data loaded from file or inline JSON.
///
/// Daily memory files (`memory/*.md`) are NOT injected — they are accessed
/// on-demand via `memory_recall` / `memory_search` tools.
pub(crate) fn build_system_prompt(
    workspace_dir: &std::path::Path,
    model_name: &str,
    _tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
) -> String {
    let context = PromptContext {
        workspace_dir,
        model_name,
        skills,
        identity_config,
        bootstrap_max_chars,
        include_channel_capabilities: true,
    };

    SystemPromptBuilder::with_defaults()
        .build(&context)
        .map(|prompt| {
            if prompt.trim().is_empty() {
                "You are ZeroClaw, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct.".to_string()
            } else {
                prompt
            }
        })
        .unwrap_or_else(|_| {
            "You are ZeroClaw, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct.".to_string()
        })
}

fn normalize_telegram_identity(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

fn bind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!(
            "Telegram channel is not configured. Run `zeroclaw onboard --channels-only` first"
        );
    };

    if telegram.allowed_users.iter().any(|u| u == "*") {
        println!(
            "⚠️ Telegram allowlist is currently wildcard (`*`) — binding is unnecessary until you remove '*'."
        );
    }

    if telegram
        .allowed_users
        .iter()
        .map(|entry| normalize_telegram_identity(entry))
        .any(|entry| entry == normalized)
    {
        println!("✅ Telegram identity already bound: {normalized}");
        return Ok(());
    }

    telegram.allowed_users.push(normalized.clone());
    updated.save()?;
    println!("✅ Bound Telegram identity: {normalized}");
    println!("   Saved to {}", updated.config_path.display());
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            println!("🔄 Detected running managed daemon service; reloaded automatically.");
        }
        Ok(false) => {
            println!(
                "ℹ️ No managed daemon service detected. If `zeroclaw daemon`/`channel start` is already running, restart it to load the updated allowlist."
            );
        }
        Err(e) => {
            eprintln!(
                "⚠️ Allowlist saved, but failed to reload daemon service automatically: {e}\n\
                 Restart service manually with `zeroclaw service stop && zeroclaw service start`."
            );
        }
    }
    Ok(())
}

fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("com.zeroclaw.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .output()
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.zeroclaw.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl")
            .args(["stop", "com.zeroclaw.daemon"])
            .output();
        let start_output = Command::new("launchctl")
            .args(["start", "com.zeroclaw.daemon"])
            .output()
            .context("Failed to start launchd daemon service")?;
        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            anyhow::bail!("launchctl start failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    if cfg!(target_os = "linux") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let unit_path: PathBuf = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("zeroclaw.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(["--user", "is-active", "zeroclaw.service"])
            .output()
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(["--user", "restart", "zeroclaw.service"])
            .output()
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

pub fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        crate::ChannelCommands::Start => {
            anyhow::bail!("Start must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::Doctor => {
            anyhow::bail!("Doctor must be handled in main.rs (requires async runtime)")
        }
        crate::ChannelCommands::List => {
            println!("Channels:");
            println!("  ✅ CLI (always available)");
            for (name, configured) in [
                ("Telegram", config.channels_config.telegram.is_some()),
                ("Discord", config.channels_config.discord.is_some()),
                ("Slack", config.channels_config.slack.is_some()),
                ("Webhook", config.channels_config.webhook.is_some()),
                ("iMessage", config.channels_config.imessage.is_some()),
                ("Matrix", config.channels_config.matrix.is_some()),
                ("Signal", config.channels_config.signal.is_some()),
                ("WhatsApp", config.channels_config.whatsapp.is_some()),
                ("Email", config.channels_config.email.is_some()),
                ("IRC", config.channels_config.irc.is_some()),
                ("Lark", config.channels_config.lark.is_some()),
                ("DingTalk", config.channels_config.dingtalk.is_some()),
                ("QQ", config.channels_config.qq.is_some()),
            ] {
                println!("  {} {name}", if configured { "✅" } else { "❌" });
            }
            println!("\nTo start channels: zeroclaw channel start");
            println!("To check health:    zeroclaw channel doctor");
            println!("To configure:      zeroclaw onboard");
            Ok(())
        }
        crate::ChannelCommands::Add {
            channel_type,
            config: _,
        } => {
            anyhow::bail!(
                "Channel type '{channel_type}' — use `zeroclaw onboard` to configure channels"
            );
        }
        crate::ChannelCommands::Remove { name } => {
            anyhow::bail!("Remove channel '{name}' — edit ~/.zeroclaw/config.toml directly");
        }
        crate::ChannelCommands::BindTelegram { identity } => {
            bind_telegram_identity(config, &identity)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

/// Run health checks for configured channels.
pub async fn doctor_channels(config: Config) -> Result<()> {
    let mut channels: Vec<(&'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push((
            "Telegram",
            Arc::new(TelegramChannel::new(
                tg.bot_token.clone(),
                tg.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push((
            "Discord",
            Arc::new(DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            )),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push((
            "Slack",
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push((
            "iMessage",
            Arc::new(IMessageChannel::new(im.allowed_contacts.clone())),
        ));
    }

    if let Some(ref mx) = config.channels_config.matrix {
        channels.push((
            "Matrix",
            Arc::new(MatrixChannel::new(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref sig) = config.channels_config.signal {
        channels.push((
            "Signal",
            Arc::new(SignalChannel::new(
                sig.http_url.clone(),
                sig.account.clone(),
                sig.group_id.clone(),
                sig.allowed_from.clone(),
                sig.ignore_attachments,
                sig.ignore_stories,
            )),
        ));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        channels.push((
            "WhatsApp",
            Arc::new(WhatsAppChannel::new(
                wa.access_token.clone(),
                wa.phone_number_id.clone(),
                wa.verify_token.clone(),
                wa.allowed_numbers.clone(),
            )),
        ));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(("Email", Arc::new(EmailChannel::new(email_cfg.clone()))));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push((
            "IRC",
            Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
            })),
        ));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(("Lark", Arc::new(LarkChannel::from_config(lk))));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push((
            "DingTalk",
            Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push((
            "QQ",
            Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        ));
    }

    if channels.is_empty() {
        println!("No real-time channels configured. Run `zeroclaw onboard` first.");
        return Ok(());
    }

    println!("🩺 ZeroClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (name, channel) in channels {
        let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} healthy");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} unhealthy (auth/config/network)");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} timed out (>10s)");
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `zeroclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}

/// Start all configured channels and route messages to the agent
#[allow(clippy::too_many_lines)]
pub async fn start_channels(config: Config) -> Result<()> {
    let manager = providers::ProviderManager::new(&config)?;
    let default_resolved = manager.default_resolved().map_err(|e| anyhow::anyhow!("Default provider failed: {e}"))?;
    let default_full_model = manager.default_full_model().to_string();
    let provider_manager: Arc<dyn ProviderManagerTrait> = Arc::new(manager);
    if let Err(e) = default_resolved.provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    let observer: Arc<dyn Observer> =
        Arc::from(observability::create_observer(&config.observability));
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory(
        &config.memory,
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?);
    let session_store = if config.session.enabled {
        Some(Arc::new(SessionStore::new(&config.workspace_dir)?))
    } else {
        None
    };
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    // Build system prompt from workspace identity files + skills
    let workspace = config.workspace_dir.clone();
    let tools_registry = Arc::new(tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &workspace,
        &config,
    ));

    let skills = crate::skills::load_skills(&workspace);

    let tool_prompt_entries: Vec<(&str, &str)> = tools_registry
        .iter()
        .map(|tool| (tool.name(), tool.description()))
        .collect();

    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let system_prompt = build_system_prompt(
        &workspace,
        &default_full_model,
        &tool_prompt_entries,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
    );
    if !skills.is_empty() {
        println!(
            "  🧩 Skills:   {}",
            skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Collect active channels
    let mut channels: Vec<Arc<dyn Channel>> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push(Arc::new(TelegramChannel::new(
            tg.bot_token.clone(),
            tg.allowed_users.clone(),
        )));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push(Arc::new(DiscordChannel::new(
            dc.bot_token.clone(),
            dc.guild_id.clone(),
            dc.allowed_users.clone(),
            dc.listen_to_bots,
            dc.mention_only,
        )));
    }

    if let Some(ref sl) = config.channels_config.slack {
        channels.push(Arc::new(SlackChannel::new(
            sl.bot_token.clone(),
            sl.channel_id.clone(),
            sl.allowed_users.clone(),
        )));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push(Arc::new(MattermostChannel::new(
            mm.url.clone(),
            mm.bot_token.clone(),
            mm.channel_id.clone(),
            mm.allowed_users.clone(),
        )));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push(Arc::new(IMessageChannel::new(im.allowed_contacts.clone())));
    }

    if let Some(ref mx) = config.channels_config.matrix {
        channels.push(Arc::new(MatrixChannel::new(
            mx.homeserver.clone(),
            mx.access_token.clone(),
            mx.room_id.clone(),
            mx.allowed_users.clone(),
        )));
    }

    if let Some(ref sig) = config.channels_config.signal {
        channels.push(Arc::new(SignalChannel::new(
            sig.http_url.clone(),
            sig.account.clone(),
            sig.group_id.clone(),
            sig.allowed_from.clone(),
            sig.ignore_attachments,
            sig.ignore_stories,
        )));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        channels.push(Arc::new(WhatsAppChannel::new(
            wa.access_token.clone(),
            wa.phone_number_id.clone(),
            wa.verify_token.clone(),
            wa.allowed_numbers.clone(),
        )));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push(Arc::new(EmailChannel::new(email_cfg.clone())));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push(Arc::new(IrcChannel::new(irc::IrcChannelConfig {
            server: irc.server.clone(),
            port: irc.port,
            nickname: irc.nickname.clone(),
            username: irc.username.clone(),
            channels: irc.channels.clone(),
            allowed_users: irc.allowed_users.clone(),
            server_password: irc.server_password.clone(),
            nickserv_password: irc.nickserv_password.clone(),
            sasl_password: irc.sasl_password.clone(),
            verify_tls: irc.verify_tls.unwrap_or(true),
        })));
    }

    if let Some(ref lk) = config.channels_config.lark {
        channels.push(Arc::new(LarkChannel::from_config(lk)));
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push(Arc::new(DingTalkChannel::new(
            dt.client_id.clone(),
            dt.client_secret.clone(),
            dt.allowed_users.clone(),
        )));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push(Arc::new(QQChannel::new(
            qq.app_id.clone(),
            qq.app_secret.clone(),
            qq.allowed_users.clone(),
        )));
    }

    if channels.is_empty() {
        println!("No channels configured. Run `zeroclaw onboard` to set up channels.");
        return Ok(());
    }

    println!("🦀 ZeroClaw Channel Server");
    println!("  🤖 Model:    {default_full_model}");
    println!(
        "  🧠 Memory:   {} (auto-save: {})",
        config.memory.backend,
        if config.memory.auto_save { "on" } else { "off" }
    );
    println!(
        "  📡 Channels: {}",
        channels
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    println!("  Listening for messages... (Ctrl+C to stop)");
    println!();

    crate::health::mark_component_ok("channels");

    let initial_backoff_secs = config
        .reliability
        .channel_initial_backoff_secs
        .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
    let max_backoff_secs = config
        .reliability
        .channel_max_backoff_secs
        .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

    // Single message bus — all channels send messages here
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(100);
    set_internal_dispatch_sender(Some(tx.clone()));

    // Spawn a listener for each channel
    let mut handles = Vec::new();
    for ch in &channels {
        handles.push(spawn_supervised_listener(
            ch.clone(),
            tx.clone(),
            initial_backoff_secs,
            max_backoff_secs,
        ));
    }
    drop(tx); // Drop our copy so rx closes when all channels stop

    let channels_by_name_map = channels
        .iter()
        .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
        .collect::<HashMap<_, _>>();
    let channels_by_name = Arc::new(channels_by_name_map);
    let max_in_flight_messages = compute_max_in_flight_messages(channels.len());

    println!("  🚦 In-flight message limit: {max_in_flight_messages}");

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        channels_by_name,
        provider_manager: Arc::clone(&provider_manager),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        system_prompt: Arc::new(system_prompt),
        auto_save_memory: config.memory.auto_save,
        session_enabled: config.session.enabled,
        session_history_limit: config.session.history_limit,
        session_store,
        session_resolver: SessionResolver::new(),
        config: Arc::new(config.clone()),
        all_skills: Arc::new(skills),
    });

    run_message_dispatch_loop(rx, runtime_ctx, max_in_flight_messages).await;
    set_internal_dispatch_sender(None);

    // Wait for all channel tasks
    for h in handles {
        let _ = h.await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::ChatType;
    use crate::config::Config;
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use crate::observability::NoopObserver;
    use crate::providers::traits::ProviderCapabilities;
    use crate::providers::{ChatRequest, ChatResponse, Provider, ProviderManager, ProviderManagerTrait, ToolCall};
    use crate::tools::{Tool, ToolResult};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_workspace() -> TempDir {
        let tmp = TempDir::new().unwrap();
        // Create minimal workspace files
        std::fs::write(tmp.path().join("SOUL.md"), "# Soul\nBe helpful.").unwrap();
        std::fs::write(tmp.path().join("IDENTITY.md"), "# Identity\nName: ZeroClaw").unwrap();
        std::fs::write(tmp.path().join("USER.md"), "# User\nName: Test User").unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "# Agents\nFollow instructions.",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("HEARTBEAT.md"),
            "# Heartbeat\nCheck status.",
        )
        .unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Memory\nUser likes Rust.").unwrap();
        tmp
    }

    #[derive(Default)]
    struct RecordingChannel {
        sent_messages: tokio::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "test-channel"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .await
                .push(format!("{}:{}", message.recipient, message.content));
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct SlowProvider {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            tokio::time::sleep(self.delay).await;
            let message = request
                .messages
                .iter()
                .rfind(|m| m.role == "user")
                .map(|m| m.content.as_str())
                .unwrap_or_default();
            Ok(ChatResponse {
                text: Some(format!("echo: {message}")),
                tool_calls: vec![],
            })
        }
    }

    struct ToolCallingProvider;

    #[async_trait::async_trait]
    impl Provider for ToolCallingProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
            }
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let has_tool_results = request
                .messages
                .iter()
                .any(|msg| msg.role == "tool");
            if has_tool_results {
                Ok(ChatResponse {
                    text: Some("BTC is currently around $65,000 based on latest tool output.".to_string()),
                    tool_calls: vec![],
                })
            } else {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "tc-mock-1".to_string(),
                        name: "mock_price".to_string(),
                        arguments: r#"{"symbol":"BTC"}"#.to_string(),
                    }],
                })
            }
        }
    }

    struct ToolCallingAliasProvider;

    #[async_trait::async_trait]
    impl Provider for ToolCallingAliasProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
            }
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let has_tool_results = request
                .messages
                .iter()
                .any(|msg| msg.role == "tool");
            if has_tool_results {
                Ok(ChatResponse {
                    text: Some("BTC alias-tag flow resolved to final text output.".to_string()),
                    tool_calls: vec![],
                })
            } else {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "tc-alias-1".to_string(),
                        name: "mock_price".to_string(),
                        arguments: r#"{"symbol":"BTC"}"#.to_string(),
                    }],
                })
            }
        }
    }

    struct MockPriceTool;

    #[async_trait::async_trait]
    impl Tool for MockPriceTool {
        fn name(&self) -> &str {
            "mock_price"
        }

        fn description(&self) -> &str {
            "Return a mocked BTC price"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string" }
                },
                "required": ["symbol"]
            })
        }

        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let symbol = args.get("symbol").and_then(serde_json::Value::as_str);
            if symbol != Some("BTC") {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("unexpected symbol".to_string()),
                });
            }

            Ok(ToolResult {
                success: true,
                output: r#"{"symbol":"BTC","price_usd":65000}"#.to_string(),
                error: None,
            })
        }
    }

    /// Test double: returns the same provider for any get() and for default_resolved().
    struct TestProviderManager(Arc<dyn Provider>, String, f64);
    impl ProviderManagerTrait for TestProviderManager {
        fn get(
            &self,
            full_model: &str,
            temperature: f64,
        ) -> anyhow::Result<ProviderCtx> {
            let model = full_model
                .split_once('/')
                .map(|(_, m)| m.trim().to_string())
                .unwrap_or_else(|| full_model.to_string());
            Ok(ProviderCtx {
                provider: Arc::clone(&self.0),
                model,
                temperature,
            })
        }
        fn default_resolved(&self) -> anyhow::Result<ProviderCtx> {
            let model = self
                .1
                .split_once('/')
                .map(|(_, m)| m.trim().to_string())
                .unwrap_or_else(|| self.1.clone());
            Ok(ProviderCtx {
                provider: Arc::clone(&self.0),
                model,
                temperature: self.2,
            })
        }
        fn default_full_model(&self) -> &str {
            &self.1
        }
    }

    fn session_test_message(content: &str, id: &str) -> traits::ChannelMessage {
        traits::ChannelMessage {
            id: id.to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user".to_string(),
            reply_target: "chat-session".to_string(),
            content: content.to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-session".to_string(),
            thread_id: None,
            timestamp: 1,
        }
    }

    fn session_runtime_ctx(
        session_store: Arc<SessionStore>,
        channel: Arc<dyn Channel>,
    ) -> Arc<ChannelRuntimeContext> {
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let config = Config::default();
        let manager = ProviderManager::new(&config).unwrap();
        let provider_manager: Arc<dyn ProviderManagerTrait> = Arc::new(manager);
        Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager,
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store),
            session_resolver: SessionResolver::new(),
            config: Arc::new(config),
            all_skills: Arc::new(vec![]),
        })
    }

    #[test]
    fn should_deliver_to_external_channel_returns_true_when_no_store_or_session() {
        assert!(should_deliver_to_external_channel(None, None));
    }

    #[test]
    fn should_deliver_to_external_channel_returns_false_when_session_channel_is_internal() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let session_key = crate::session::SessionKey::new("group:test:internal");
        let session_id = store.get_or_create_active(&session_key).unwrap();
        store
            .upsert_route_metadata(
                &session_id,
                &SessionRouteMetadata {
                    agent_id: None,
                    channel: INTERNAL_MESSAGE_CHANNEL.to_string(),
                    account_id: None,
                    chat_type: "direct".to_string(),
                    chat_id: "chat-internal".to_string(),
                    route_id: None,
                    sender_id: "zeroclaw_user".to_string(),
                    title: None,
                },
            )
            .unwrap();
        assert!(!should_deliver_to_external_channel(
            Some(&store),
            Some(&session_id)
        ));
    }

    #[test]
    fn should_deliver_to_external_channel_returns_true_when_session_channel_is_telegram() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let session_key = crate::session::SessionKey::new("group:telegram:deliver");
        let session_id = store.get_or_create_active(&session_key).unwrap();
        store
            .upsert_route_metadata(
                &session_id,
                &SessionRouteMetadata {
                    agent_id: None,
                    channel: "telegram".to_string(),
                    account_id: None,
                    chat_type: "direct".to_string(),
                    chat_id: "chat-123".to_string(),
                    route_id: None,
                    sender_id: "zeroclaw_user".to_string(),
                    title: None,
                },
            )
            .unwrap();
        assert!(should_deliver_to_external_channel(
            Some(&store),
            Some(&session_id)
        ));
    }

    #[test]
    fn build_ephemeral_announce_context_returns_empty_for_empty_messages() {
        let messages: Vec<SessionMessage> = vec![];
        assert!(build_ephemeral_announce_context(&messages).is_empty());
    }

    #[test]
    fn build_ephemeral_announce_context_returns_empty_when_no_announce_meta() {
        let messages = vec![
            SessionMessage {
                id: 1,
                role: "assistant".to_string(),
                content: "hello".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                meta_json: None,
            },
            SessionMessage {
                id: 2,
                role: "user".to_string(),
                content: "hi".to_string(),
                created_at: "2026-01-01T00:00:01Z".to_string(),
                meta_json: None,
            },
        ];
        assert!(build_ephemeral_announce_context(&messages).is_empty());
    }

    #[test]
    fn build_ephemeral_announce_context_formats_announce_messages() {
        let messages = vec![SessionMessage {
            id: 1,
            role: "assistant".to_string(),
            content: "[@agent:runner#spec-1] finish".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            meta_json: Some(r#"{"source":{"agent_id":"spec-1"},"result":"done"}"#.to_string()),
        }];
        let out = build_ephemeral_announce_context(&messages);
        assert!(out.contains("[Subagent results]"));
        assert!(out.contains("@spec-1:"));
        assert!(out.contains("done"));
    }

    #[test]
    fn parse_agent_spec_defaults_parses_provider_model_temperature() {
        let defaults = parse_agent_spec_defaults(
            r#"{"defaults":{"provider":"openrouter","model":"anthropic/claude-3","temperature":0.2}}"#,
        );
        assert_eq!(defaults.provider, Some("openrouter".to_string()));
        assert_eq!(defaults.model, Some("anthropic/claude-3".to_string()));
        assert_eq!(defaults.temperature, Some(0.2));
    }

    #[test]
    fn parse_agent_spec_defaults_returns_default_for_invalid_json() {
        let defaults = parse_agent_spec_defaults("not json");
        assert!(defaults.provider.is_none());
        assert!(defaults.model.is_none());
    }

    #[test]
    fn parse_agent_spec_policy_parses_tools_and_skills_allow_lists() {
        let policy = parse_agent_spec_policy(
            r#"{"policy":{"tools":["shell","file_read"],"skills":["search"]}}"#,
        );
        assert_eq!(
            policy.tools,
            Some(vec!["shell".to_string(), "file_read".to_string()])
        );
        assert_eq!(policy.skills, Some(vec!["search".to_string()]));
    }

    #[test]
    fn parse_agent_spec_policy_returns_default_for_invalid_json() {
        let policy = parse_agent_spec_policy("not json");
        assert!(policy.tools.is_none());
        assert!(policy.skills.is_none());
    }

    #[test]
    fn agent_work_item_from_message_preserves_content_and_reply_target() {
        let msg = traits::ChannelMessage {
            id: "id-1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "alice".to_string(),
            reply_target: "chat-99".to_string(),
            content: "hello world".to_string(),
            channel: "telegram".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-99".to_string(),
            thread_id: None,
            timestamp: 1,
        };
        let work = AgentWorkItem::from_message(&msg);
        let back = work.to_channel_message();
        assert_eq!(back.content, "hello world");
        assert_eq!(back.reply_target, "chat-99");
        assert_eq!(back.channel, "telegram");
        assert_eq!(back.sender, "alice");
    }

    #[tokio::test]
    async fn drain_agent_queue_returns_all_items_in_order() {
        let (tx, mut rx) = mpsc::channel(4);
        let msg1 = traits::ChannelMessage {
            id: "i1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "u1".to_string(),
            reply_target: "r1".to_string(),
            content: "one".to_string(),
            channel: "ch".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "c1".to_string(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "i2".to_string(),
            agent_id: None,
            account_id: None,
            sender: "u2".to_string(),
            reply_target: "r2".to_string(),
            content: "two".to_string(),
            channel: "ch".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "c2".to_string(),
            thread_id: None,
            timestamp: 2,
        };
        let msg3 = traits::ChannelMessage {
            id: "i3".to_string(),
            agent_id: None,
            account_id: None,
            sender: "u3".to_string(),
            reply_target: "target-last".to_string(),
            content: "three".to_string(),
            channel: "ch".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "c3".to_string(),
            thread_id: None,
            timestamp: 3,
        };
        let _ = tx.send(AgentWorkItem::from_message(&msg1)).await;
        let _ = tx.send(AgentWorkItem::from_message(&msg2)).await;
        let _ = tx.send(AgentWorkItem::from_message(&msg3)).await;
        drop(tx);

        let drained = drain_agent_queue(&mut rx);
        assert_eq!(drained.len(), 3);
        let m = merge_work_items(drained).expect("merge should return one merged item");
        assert_eq!(m.content, "one\n\ntwo\n\nthree");
        assert_eq!(m.reply_target, "target-last");
        assert_eq!(m.sender, "u3");
    }

    #[tokio::test]
    async fn drain_agent_queue_returns_empty_when_empty() {
        let (_tx, mut rx) = mpsc::channel::<AgentWorkItem>(2);
        let drained = drain_agent_queue(&mut rx);
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn command_new_creates_new_active_session_without_persisting_command_message() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let msg = session_test_message("/new", "cmd-new");
        let session_key = SessionResolver::new().resolve(&normalize_session_context(&msg));
        let previous_session_id = session_store.get_or_create_active(&session_key).unwrap();

        process_channel_message(session_runtime_ctx(session_store.clone(), channel), msg).await;

        let active_after = session_store.get_or_create_active(&session_key).unwrap();
        assert_ne!(active_after.as_str(), previous_session_id.as_str());
        let previous_messages = session_store
            .load_recent_messages(&previous_session_id, 10)
            .unwrap();
        let active_messages = session_store
            .load_recent_messages(&active_after, 10)
            .unwrap();
        assert!(previous_messages.is_empty());
        assert!(active_messages.is_empty());

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Started a new session"));
    }

    #[tokio::test]
    async fn command_compact_returns_confirmation_without_persisting_command_message() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let msg = session_test_message("/compact", "cmd-compact");
        let session_key = SessionResolver::new().resolve(&normalize_session_context(&msg));
        let session_id = session_store.get_or_create_active(&session_key).unwrap();

        process_channel_message(session_runtime_ctx(session_store.clone(), channel), msg).await;

        let messages = session_store.load_recent_messages(&session_id, 10).unwrap();
        assert!(messages.is_empty());
        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("No compaction needed"));
    }

    #[tokio::test]
    async fn command_compact_in_memory_context_returns_unsupported_message() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider = Arc::new(SlowProvider {
            delay: Duration::from_millis(1),
        }) as Arc<dyn Provider>;
        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: false,
            session_history_limit: 40,
            session_store: None,
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(
            runtime_ctx,
            session_test_message("/compact", "cmd-compact-mem"),
        )
        .await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("only supported in session context"));
    }

    #[tokio::test]
    async fn command_queue_sets_mode_and_rejects_invalid_modes() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let runtime_ctx = session_runtime_ctx(session_store.clone(), channel);
        let base_msg = session_test_message("hello", "seed");
        let session_key = SessionResolver::new().resolve(&normalize_session_context(&base_msg));
        let session_id = session_store.get_or_create_active(&session_key).unwrap();

        process_channel_message(
            runtime_ctx.clone(),
            session_test_message("/queue steer-merge", "cmd-queue-1"),
        )
        .await;
        process_channel_message(
            runtime_ctx,
            session_test_message("/queue fifo", "cmd-queue-2"),
        )
        .await;

        let stored_mode = decode_session_string_state(
            session_store
                .get_state_key(&session_id, crate::agent::command::SESSION_QUEUE_MODE_KEY)
                .unwrap(),
        );
        assert_eq!(stored_mode.as_deref(), Some("steer-merge"));

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains("`steer-merge`"));
        assert!(sent[1].contains("Unsupported queue mode"));
    }

    #[tokio::test]
    async fn command_sessions_lists_current_session_id() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let msg = session_test_message("/sessions", "cmd-sessions");
        let session_key = SessionResolver::new().resolve(&normalize_session_context(&msg));
        let session_id = session_store.get_or_create_active(&session_key).unwrap();

        process_channel_message(session_runtime_ctx(session_store, channel), msg).await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains(session_id.as_str()));
        assert!(sent[0].contains("Sessions for key"));
    }

    #[tokio::test]
    async fn command_subagents_lists_specs_sessions_and_runs() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let spec = session_store
            .upsert_subagent_spec("reviewer", r#"{"model":"test"}"#)
            .unwrap();
        let _subagent_session = session_store
            .create_subagent_session(Some(spec.spec_id.as_str()), None)
            .unwrap();

        process_channel_message(
            session_runtime_ctx(session_store, channel),
            session_test_message("/subagents", "cmd-subagents"),
        )
        .await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Subagents"));
        assert!(sent[0].contains("specs: 1"));
        assert!(sent[0].contains("sessions: 1"));
    }

    #[tokio::test]
    async fn process_channel_message_executes_tool_calls_instead_of_sending_raw_json() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider = Arc::new(ToolCallingProvider) as Arc<dyn Provider>;
        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: false,
            session_history_limit: 40,
            session_store: None,
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-1".to_string(),
                agent_id: None,
                account_id: None,
                sender: "alice".to_string(),
                reply_target: "chat-42".to_string(),
                content: "What is the BTC price now?".to_string(),
                channel: "test-channel".to_string(),
                title: None,
                chat_type: ChatType::Group,
                raw_chat_type: None,
                chat_id: "chat-42".to_string(),
                thread_id: None,
                timestamp: 1,
            },
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-42:"));
        assert!(sent_messages[0].contains("BTC is currently around"));
        assert!(!sent_messages[0].contains("\"tool_calls\""));
        assert!(!sent_messages[0].contains("mock_price"));
    }

    #[tokio::test]
    async fn process_channel_message_executes_tool_calls_with_alias_tags() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider = Arc::new(ToolCallingAliasProvider) as Arc<dyn Provider>;
        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: false,
            session_history_limit: 40,
            session_store: None,
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-2".to_string(),
                agent_id: None,
                account_id: None,
                sender: "bob".to_string(),
                reply_target: "chat-84".to_string(),
                content: "What is the BTC price now?".to_string(),
                channel: "test-channel".to_string(),
                title: None,
                chat_type: ChatType::Group,
                raw_chat_type: None,
                chat_id: "chat-84".to_string(),
                thread_id: None,
                timestamp: 2,
            },
        )
        .await;

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].starts_with("chat-84:"));
        assert!(sent_messages[0].contains("alias-tag flow resolved"));
        assert!(!sent_messages[0].contains("<toolcall>"));
        assert!(!sent_messages[0].contains("mock_price"));
    }

    struct NoopMemory;

    #[async_trait::async_trait]
    impl Memory for NoopMemory {
        fn name(&self) -> &str {
            "noop"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct TrackingMemory {
        recall_calls: AtomicUsize,
        conversation_store_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Memory for TrackingMemory {
        fn name(&self) -> &str {
            "tracking"
        }

        async fn store(
            &self,
            _key: &str,
            _content: &str,
            category: crate::memory::MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            if matches!(category, crate::memory::MemoryCategory::Conversation) {
                self.conversation_store_calls
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.recall_calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![crate::memory::MemoryEntry {
                id: "id-1".to_string(),
                key: "k".to_string(),
                content: "memory-fact".to_string(),
                category: crate::memory::MemoryCategory::Conversation,
                timestamp: "1970-01-01T00:00:00Z".to_string(),
                score: Some(0.99),
                session_id: None,
            }])
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct HistoryCaptureProvider {
        captured_history: tokio::sync::Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl Provider for HistoryCaptureProvider {
        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let captured = request
                .messages
                .iter()
                .map(|msg| (msg.role.clone(), msg.content.clone()))
                .collect::<Vec<_>>();
            let mut lock = self.captured_history.lock().await;
            *lock = captured;
            Ok(ChatResponse {
                text: Some("ok".to_string()),
                tool_calls: vec![],
            })
        }
    }

    /// Provider for steer-merge test: first call delays then returns tool call; later calls
    /// return text based on the merged user message.
    struct SteerTestProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl Default for SteerTestProvider {
        fn default() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for SteerTestProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
            }
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let n = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let last_user = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let has_tool_results = request.messages.iter().any(|m| m.role == "tool");

            if n == 0 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "tc-steer-1".to_string(),
                        name: "mock_price".to_string(),
                        arguments: r#"{"symbol":"BTC"}"#.to_string(),
                    }],
                })
            } else if last_user.contains("<messages>") {
                Ok(ChatResponse {
                    text: Some("reply to steer merge".to_string()),
                    tool_calls: vec![],
                })
            } else if has_tool_results {
                Ok(ChatResponse {
                    text: Some("reply to tool results".to_string()),
                    tool_calls: vec![],
                })
            } else if last_user.contains("trigger tool") {
                Ok(ChatResponse {
                    text: Some("reply to trigger tool".to_string()),
                    tool_calls: vec![],
                })
            } else {
                Ok(ChatResponse {
                    text: Some("reply to new priority".to_string()),
                    tool_calls: vec![],
                })
            }
        }
    }

    #[tokio::test]
    async fn integration_get_or_create_agent_same_key_uses_same_agent_queue() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let provider = Arc::new(HistoryCaptureProvider::default());
        let provider_dyn = Arc::clone(&provider) as Arc<dyn Provider>;
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider_dyn,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test".to_string()),
            auto_save_memory: false,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store),
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        let msg1 = traits::ChannelMessage {
            id: "i1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_same_agent".to_string(),
            reply_target: "chat-same".to_string(),
            content: "first".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-same".to_string(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "i2".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_same_agent".to_string(),
            reply_target: "chat-same".to_string(),
            content: "second".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-same".to_string(),
            thread_id: None,
            timestamp: 2,
        };

        let key = SessionResolver::new().resolve(&normalize_session_context(&msg1));
        let session_id = ctx
            .session_store
            .as_ref()
            .unwrap()
            .get_or_create_active(&key)
            .unwrap();
        let tx1 = get_or_create_agent(Arc::clone(&ctx), session_id.clone());
        let tx2 = get_or_create_agent(Arc::clone(&ctx), session_id);
        let _ = tx1.send(AgentWorkItem::from_message(&msg1)).await;
        let _ = tx2.send(AgentWorkItem::from_message(&msg2)).await;
        drop(tx1);
        drop(tx2);

        tokio::time::sleep(Duration::from_millis(500)).await;

        let history = provider.captured_history.lock().await;
        let user_contents: Vec<&str> = history
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, c)| c.as_str())
            .collect();
        let has_first = user_contents.iter().any(|c| c.contains("first"));
        let has_second = user_contents.iter().any(|c| c.contains("second"));
        assert!(
            has_first && has_second,
            "same agent should process both messages (possibly merged); got user messages: {:?}",
            user_contents
        );
    }

    #[tokio::test]
    async fn integration_agent_unregisters_when_queue_sender_dropped() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel.clone());
        let provider = Arc::new(SlowProvider {
            delay: Duration::from_millis(1),
        }) as Arc<dyn Provider>;
        let ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test".to_string()),
            auto_save_memory: false,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store),
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        let msg1 = traits::ChannelMessage {
            id: "u1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_unreg".to_string(),
            reply_target: "chat-unreg".to_string(),
            content: "first".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-unreg".to_string(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "u2".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_unreg".to_string(),
            reply_target: "chat-unreg".to_string(),
            content: "second".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-unreg".to_string(),
            thread_id: None,
            timestamp: 2,
        };

        let key = SessionResolver::new().resolve(&normalize_session_context(&msg1));
        let session_id = ctx
            .session_store
            .as_ref()
            .unwrap()
            .get_or_create_active(&key)
            .unwrap();
        let tx1 = get_or_create_agent(Arc::clone(&ctx), session_id.clone());
        let _ = tx1.send(AgentWorkItem::from_message(&msg1)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(tx1);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let tx2 = get_or_create_agent(ctx, session_id);
        let _ = tx2.send(AgentWorkItem::from_message(&msg2)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(
            sent.len(),
            2,
            "after unregister, new agent should process second message; got {} replies",
            sent.len()
        );
    }

    #[tokio::test]
    async fn integration_steer_merge_combines_current_and_pending_messages() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let provider = Arc::new(SteerTestProvider::default()) as Arc<dyn Provider>;
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();
        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel.clone());

        let ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                Arc::clone(&provider),
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test".to_string()),
            auto_save_memory: false,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store),
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        let msg1 = traits::ChannelMessage {
            id: "steer-1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_steer".to_string(),
            reply_target: "chat-steer".to_string(),
            content: "trigger tool".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-steer".to_string(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "steer-2".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_steer".to_string(),
            reply_target: "chat-steer".to_string(),
            content: "new priority".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-steer".to_string(),
            thread_id: None,
            timestamp: 2,
        };

        let key = SessionResolver::new().resolve(&normalize_session_context(&msg1));
        let session_id = ctx
            .session_store
            .as_ref()
            .unwrap()
            .get_or_create_active(&key)
            .unwrap();
        let tx = get_or_create_agent(Arc::clone(&ctx), session_id);
        let _ = tx.send(AgentWorkItem::from_message(&msg1)).await;
        tokio::task::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(AgentWorkItem::from_message(&msg2)).await;
        });
        tokio::time::sleep(Duration::from_millis(600)).await;

        let sent = channel_impl.sent_messages.lock().await;
        assert_eq!(
            sent.len(),
            1,
            "steer-merge should produce a single merged turn"
        );
        let reply = sent.first().map(String::as_str).unwrap_or("");
        assert!(
            reply.contains("reply to steer merge"),
            "expected steer-merge reply; got: {}",
            reply
        );
    }

    #[tokio::test]
    async fn process_channel_message_session_mode_skips_memory_context_and_conversation_autosave() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let memory = Arc::new(TrackingMemory::default());
        let provider = Arc::new(HistoryCaptureProvider::default());
        let provider_dyn = Arc::clone(&provider) as Arc<dyn Provider>;
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let msg = traits::ChannelMessage {
            id: "msg-3".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_skip_memory".to_string(),
            reply_target: "chat-skip-mem".to_string(),
            content: "current question".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-skip-mem".to_string(),
            thread_id: None,
            timestamp: 3,
        };

        let session_resolver = SessionResolver::new();
        let session_key = session_resolver.resolve(&normalize_session_context(&msg));
        let session_id = session_store.get_or_create_active(&session_key).unwrap();
        session_store
            .append_message(&session_id, "user", "past-user", None)
            .unwrap();
        session_store
            .append_message(&session_id, "assistant", "past-assistant", None)
            .unwrap();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider_dyn,
                "test/model".to_string(),
                0.0,
            )),
            memory: memory.clone(),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: true,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store.clone()),
            session_resolver,
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(runtime_ctx, msg).await;
        // Agent loop runs in a spawned task; give it time to process the enqueued message.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(memory.recall_calls.load(Ordering::Relaxed), 0);
        assert_eq!(memory.conversation_store_calls.load(Ordering::Relaxed), 0);

        let captured_history = provider.captured_history.lock().await;
        let final_user = captured_history
            .iter()
            .rev()
            .find(|(role, _)| role == "user")
            .map(|(_, content)| content.clone())
            .unwrap();
        assert_eq!(final_user, "current question");
        assert!(captured_history
            .iter()
            .any(|(role, content)| role == "user" && content == "past-user"));
        assert!(captured_history
            .iter()
            .any(|(role, content)| role == "assistant" && content == "past-assistant"));
    }

    #[tokio::test]
    async fn process_channel_message_session_mode_uses_compaction_summary_and_boundary_tail() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let memory = Arc::new(TrackingMemory::default());
        let provider = Arc::new(HistoryCaptureProvider::default());
        let provider_dyn = Arc::clone(&provider) as Arc<dyn Provider>;
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let msg = traits::ChannelMessage {
            id: "msg-4".to_string(),
            agent_id: None,
            account_id: None,
            sender: "zeroclaw_user_compaction_tail".to_string(),
            reply_target: "chat-200".to_string(),
            content: "latest question".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "chat-200".to_string(),
            thread_id: None,
            timestamp: 4,
        };

        let session_resolver = SessionResolver::new();
        let session_key = session_resolver.resolve(&normalize_session_context(&msg));
        let session_id = session_store.get_or_create_active(&session_key).unwrap();
        session_store
            .append_message(&session_id, "user", "old-user", None)
            .unwrap();
        session_store
            .append_message(&session_id, "assistant", "old-assistant", None)
            .unwrap();
        session_store
            .append_message(&session_id, "user", "recent-user", None)
            .unwrap();
        session_store
            .append_message(&session_id, "assistant", "recent-assistant", None)
            .unwrap();

        let all_messages = session_store
            .load_messages_after_id(&session_id, None)
            .unwrap();
        let boundary_message_id = all_messages[1].id;
        session_store
            .set_state_key(
                &session_id,
                crate::session::compaction::SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY,
                &serde_json::to_string(&boundary_message_id).unwrap(),
            )
            .unwrap();
        session_store
            .set_state_key(
                &session_id,
                crate::session::compaction::SESSION_COMPACTION_SUMMARY_KEY,
                "\"summary-v2\"",
            )
            .unwrap();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider_dyn,
                "test/model".to_string(),
                0.0,
            )),
            memory: memory.clone(),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: true,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store.clone()),
            session_resolver,
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(runtime_ctx, msg).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(memory.recall_calls.load(Ordering::Relaxed), 0);

        let captured_history = provider.captured_history.lock().await;
        assert!(captured_history.iter().any(|(role, content)| {
            role == "assistant" && content.contains("[Session Compaction Summary]")
        }));
        assert!(captured_history
            .iter()
            .any(|(role, content)| role == "assistant" && content == "recent-assistant"));
        assert!(captured_history
            .iter()
            .any(|(role, content)| role == "user" && content == "recent-user"));
        assert!(!captured_history
            .iter()
            .any(|(_, content)| content == "old-user"));
        assert!(!captured_history
            .iter()
            .any(|(_, content)| content == "old-assistant"));
    }

    #[tokio::test]
    async fn process_channel_message_session_mode_upserts_route_metadata() {
        let temp = TempDir::new().unwrap();
        let session_store = Arc::new(SessionStore::new(temp.path()).unwrap());
        let provider = Arc::new(HistoryCaptureProvider::default()) as Arc<dyn Provider>;
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                Arc::clone(&provider),
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(TrackingMemory::default()),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: true,
            session_history_limit: 40,
            session_store: Some(session_store.clone()),
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        process_channel_message(
            runtime_ctx,
            traits::ChannelMessage {
                id: "msg-meta".to_string(),
                agent_id: Some("zeroclaw-bot".to_string()),
                account_id: Some("account-main".to_string()),
                sender: "zeroclaw_user".to_string(),
                reply_target: "chat-meta".to_string(),
                content: "metadata ping".to_string(),
                channel: "test-channel".to_string(),
                title: Some("Project Alpha Group".to_string()),
                chat_type: ChatType::Group,
                raw_chat_type: None,
                chat_id: "chat-meta".to_string(),
                thread_id: Some("thread-77".to_string()),
                timestamp: 5,
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let candidates = session_store
            .find_chat_candidates_by_title("alpha", 10)
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].chat_id, "chat-meta");
        assert_eq!(candidates[0].channel, "test-channel");
        assert_eq!(candidates[0].account_id.as_deref(), Some("account-main"));
        assert_eq!(candidates[0].chat_type, "group");
    }

    #[tokio::test]
    async fn message_dispatch_processes_messages_in_parallel() {
        let channel_impl = Arc::new(RecordingChannel::default());
        let channel: Arc<dyn Channel> = channel_impl.clone();

        let mut channels_by_name = HashMap::new();
        channels_by_name.insert(channel.name().to_string(), channel);

        let provider = Arc::new(SlowProvider {
            delay: Duration::from_millis(250),
        }) as Arc<dyn Provider>;
        let runtime_ctx = Arc::new(ChannelRuntimeContext {
            channels_by_name: Arc::new(channels_by_name),
            provider_manager: Arc::new(TestProviderManager(
                provider,
                "test/model".to_string(),
                0.0,
            )),
            memory: Arc::new(NoopMemory),
            tools_registry: Arc::new(vec![]),
            observer: Arc::new(NoopObserver),
            system_prompt: Arc::new("test-system-prompt".to_string()),
            auto_save_memory: false,
            session_enabled: false,
            session_history_limit: 40,
            session_store: None,
            session_resolver: SessionResolver::new(),
            config: Arc::new(Config::default()),
            all_skills: Arc::new(vec![]),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(4);
        tx.send(traits::ChannelMessage {
            id: "1".to_string(),
            agent_id: None,
            account_id: None,
            sender: "alice".to_string(),
            reply_target: "alice".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "alice".to_string(),
            thread_id: None,
            timestamp: 1,
        })
        .await
        .unwrap();
        tx.send(traits::ChannelMessage {
            id: "2".to_string(),
            agent_id: None,
            account_id: None,
            sender: "bob".to_string(),
            reply_target: "bob".to_string(),
            content: "world".to_string(),
            channel: "test-channel".to_string(),
            title: None,
            chat_type: ChatType::Direct,
            raw_chat_type: None,
            chat_id: "bob".to_string(),
            thread_id: None,
            timestamp: 2,
        })
        .await
        .unwrap();
        drop(tx);

        let started = Instant::now();
        run_message_dispatch_loop(rx, runtime_ctx, 2).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(430),
            "expected parallel dispatch (<430ms), got {:?}",
            elapsed
        );

        let sent_messages = channel_impl.sent_messages.lock().await;
        assert_eq!(sent_messages.len(), 2);
    }

    #[test]
    fn prompt_contains_all_sections() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "test-model", &[], &[], None, None);

        // Section headers
        assert!(prompt.contains("## Safety"), "missing Safety section");
        assert!(prompt.contains("## Workspace"), "missing Workspace section");
        assert!(
            prompt.contains("## Project Context"),
            "missing Project Context"
        );
        assert!(
            prompt.contains("## Current Date & Time"),
            "missing Date/Time"
        );
        assert!(prompt.contains("## Runtime"), "missing Runtime section");
    }

    #[test]
    fn prompt_omits_tools_section() {
        let ws = make_workspace();
        let tools = vec![
            ("shell", "Run commands"),
            ("memory_recall", "Search memory"),
        ];
        let prompt = build_system_prompt(ws.path(), "gpt-4o", &tools, &[], None, None);

        assert!(!prompt.contains("## Tools"));
        assert!(!prompt.contains("## Tool Use Protocol"));
    }

    #[test]
    fn prompt_injects_safety() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("Do not exfiltrate private data"));
        assert!(prompt.contains("Do not run destructive commands"));
        assert!(prompt.contains("Prefer `trash` over `rm`"));
    }

    #[test]
    fn prompt_injects_workspace_files() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("### SOUL.md"), "missing SOUL.md header");
        assert!(prompt.contains("Be helpful"), "missing SOUL content");
        assert!(prompt.contains("### IDENTITY.md"), "missing IDENTITY.md");
        assert!(
            prompt.contains("Name: ZeroClaw"),
            "missing IDENTITY content"
        );
        assert!(prompt.contains("### USER.md"), "missing USER.md");
        assert!(prompt.contains("### AGENTS.md"), "missing AGENTS.md");
        assert!(prompt.contains("### HEARTBEAT.md"), "missing HEARTBEAT.md");
        assert!(prompt.contains("### MEMORY.md"), "missing MEMORY.md");
        assert!(prompt.contains("User likes Rust"), "missing MEMORY content");
    }

    #[test]
    fn prompt_missing_file_markers() {
        let tmp = TempDir::new().unwrap();
        // Empty workspace — no files at all
        let prompt = build_system_prompt(tmp.path(), "model", &[], &[], None, None);

        assert!(prompt.contains("[File not found: SOUL.md]"));
        assert!(prompt.contains("[File not found: AGENTS.md]"));
        assert!(prompt.contains("[File not found: IDENTITY.md]"));
    }

    #[test]
    fn prompt_bootstrap_only_if_exists() {
        let ws = make_workspace();
        // No BOOTSTRAP.md — should not appear
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);
        assert!(
            !prompt.contains("### BOOTSTRAP.md"),
            "BOOTSTRAP.md should not appear when missing"
        );

        // Create BOOTSTRAP.md — should appear
        std::fs::write(ws.path().join("BOOTSTRAP.md"), "# Bootstrap\nFirst run.").unwrap();
        let prompt2 = build_system_prompt(ws.path(), "model", &[], &[], None, None);
        assert!(
            prompt2.contains("### BOOTSTRAP.md"),
            "BOOTSTRAP.md should appear when present"
        );
        assert!(prompt2.contains("First run"));
    }

    #[test]
    fn prompt_no_daily_memory_injection() {
        let ws = make_workspace();
        let memory_dir = ws.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        std::fs::write(
            memory_dir.join(format!("{today}.md")),
            "# Daily\nSome note.",
        )
        .unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Daily notes should NOT be in the system prompt (on-demand via tools)
        assert!(
            !prompt.contains("Daily Notes"),
            "daily notes should not be auto-injected"
        );
        assert!(
            !prompt.contains("Some note"),
            "daily content should not be in prompt"
        );
    }

    #[test]
    fn prompt_runtime_metadata() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "claude-sonnet-4", &[], &[], None, None);

        assert!(prompt.contains("Model: claude-sonnet-4"));
        assert!(prompt.contains(&format!("OS: {}", std::env::consts::OS)));
        assert!(prompt.contains("Host:"));
    }

    #[test]
    fn prompt_skills_compact_list() {
        let ws = make_workspace();
        let skills = vec![crate::skills::Skill {
            name: "code-review".into(),
            description: "Review code for bugs".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Long prompt content that should NOT appear in system prompt".into()],
            location: None,
        }];

        let prompt = build_system_prompt(ws.path(), "model", &[], &skills, None, None);

        assert!(prompt.contains("<available_skills>"), "missing skills XML");
        assert!(prompt.contains("<name>code-review</name>"));
        assert!(prompt.contains("<description>Review code for bugs</description>"));
        assert!(prompt.contains("SKILL.md</location>"));
        assert!(
            prompt.contains("loaded on demand"),
            "should mention on-demand loading"
        );
        // Full prompt content should NOT be dumped
        assert!(!prompt.contains("Long prompt content that should NOT appear"));
    }

    #[test]
    fn prompt_truncation() {
        let ws = make_workspace();
        // Write a file larger than BOOTSTRAP_MAX_CHARS
        let big_content = "x".repeat(BOOTSTRAP_MAX_CHARS + 1000);
        std::fs::write(ws.path().join("AGENTS.md"), &big_content).unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(
            prompt.contains("truncated at"),
            "large files should be truncated"
        );
        assert!(
            !prompt.contains(&big_content),
            "full content should not appear"
        );
    }

    #[test]
    fn prompt_empty_files_skipped() {
        let ws = make_workspace();
        std::fs::write(ws.path().join("HEARTBEAT.md"), "").unwrap();

        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Empty file should not produce a header
        assert!(
            !prompt.contains("### HEARTBEAT.md"),
            "empty files should be skipped"
        );
    }

    #[test]
    fn channel_log_truncation_is_utf8_safe_for_multibyte_text() {
        let msg = "Hello from ZeroClaw 🌍. Current status is healthy, and café-style UTF-8 text stays safe in logs.";

        // Reproduces the production crash path where channel logs truncate at 80 chars.
        let result = std::panic::catch_unwind(|| crate::util::truncate_with_ellipsis(msg, 80));
        assert!(
            result.is_ok(),
            "truncate_with_ellipsis should never panic on UTF-8"
        );

        let truncated = result.unwrap();
        assert!(!truncated.is_empty());
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn prompt_contains_channel_capabilities() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(
            prompt.contains("## Channel Capabilities"),
            "missing Channel Capabilities section"
        );
        assert!(
            prompt.contains("running as a Discord bot"),
            "missing Discord context"
        );
        assert!(
            prompt.contains("NEVER repeat, describe, or echo credentials"),
            "missing security instruction"
        );
    }

    #[test]
    fn prompt_workspace_path() {
        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        assert!(prompt.contains(&format!("Working directory: `{}`", ws.path().display())));
    }

    #[test]
    fn conversation_memory_key_uses_message_id() {
        let msg = traits::ChannelMessage {
            id: "msg_abc123".into(),
            agent_id: None,
            account_id: None,
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "hello".into(),
            channel: "slack".into(),
            title: None,
            chat_type: ChatType::Group,
            raw_chat_type: None,
            chat_id: "C456".into(),
            thread_id: None,
            timestamp: 1,
        };

        assert_eq!(conversation_memory_key(&msg), "slack_U123_msg_abc123");
    }

    #[test]
    fn conversation_memory_key_is_unique_per_message() {
        let msg1 = traits::ChannelMessage {
            id: "msg_1".into(),
            agent_id: None,
            account_id: None,
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "first".into(),
            channel: "slack".into(),
            title: None,
            chat_type: ChatType::Group,
            raw_chat_type: None,
            chat_id: "C456".into(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "msg_2".into(),
            agent_id: None,
            account_id: None,
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "second".into(),
            channel: "slack".into(),
            title: None,
            chat_type: ChatType::Group,
            raw_chat_type: None,
            chat_id: "C456".into(),
            thread_id: None,
            timestamp: 2,
        };

        assert_ne!(
            conversation_memory_key(&msg1),
            conversation_memory_key(&msg2)
        );
    }

    #[tokio::test]
    async fn autosave_keys_preserve_multiple_conversation_facts() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();

        let msg1 = traits::ChannelMessage {
            id: "msg_1".into(),
            agent_id: None,
            account_id: None,
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "I'm Paul".into(),
            channel: "slack".into(),
            title: None,
            chat_type: ChatType::Group,
            raw_chat_type: None,
            chat_id: "C456".into(),
            thread_id: None,
            timestamp: 1,
        };
        let msg2 = traits::ChannelMessage {
            id: "msg_2".into(),
            agent_id: None,
            account_id: None,
            sender: "U123".into(),
            reply_target: "C456".into(),
            content: "I'm 45".into(),
            channel: "slack".into(),
            title: None,
            chat_type: ChatType::Group,
            raw_chat_type: None,
            chat_id: "C456".into(),
            thread_id: None,
            timestamp: 2,
        };

        mem.store(
            &conversation_memory_key(&msg1),
            &msg1.content,
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        mem.store(
            &conversation_memory_key(&msg2),
            &msg2.content,
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();

        assert_eq!(mem.count().await.unwrap(), 2);

        let recalled = mem.recall("45", 5, None).await.unwrap();
        assert!(recalled.iter().any(|entry| entry.content.contains("45")));
    }

    #[tokio::test]
    async fn build_memory_context_includes_recalled_entries() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        mem.store("age_fact", "Age is 45", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let context = build_memory_context(&mem, "age", None).await;
        assert!(context.contains("[Memory context]"));
        assert!(context.contains("Age is 45"));
    }

    // ── AIEOS Identity Tests (Issue #168) ─────────────────────────

    #[test]
    fn aieos_identity_from_file() {
        use crate::config::IdentityConfig;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let identity_path = tmp.path().join("aieos_identity.json");

        // Write AIEOS identity file
        let aieos_json = r#"{
            "identity": {
                "names": {"first": "Nova", "nickname": "Nov"},
                "bio": "A helpful AI assistant.",
                "origin": "Silicon Valley"
            },
            "psychology": {
                "mbti": "INTJ",
                "moral_compass": ["Be helpful", "Do no harm"]
            },
            "linguistics": {
                "style": "concise",
                "formality": "casual"
            }
        }"#;
        std::fs::write(&identity_path, aieos_json).unwrap();

        // Create identity config pointing to the file
        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: Some("aieos_identity.json".into()),
            aieos_inline: None,
        };

        let prompt = build_system_prompt(tmp.path(), "model", &[], &[], Some(&config), None);

        // Should contain AIEOS sections
        assert!(prompt.contains("## Identity"));
        assert!(prompt.contains("**Name:** Nova"));
        assert!(prompt.contains("**Nickname:** Nov"));
        assert!(prompt.contains("**Bio:** A helpful AI assistant."));
        assert!(prompt.contains("**Origin:** Silicon Valley"));

        assert!(prompt.contains("## Personality"));
        assert!(prompt.contains("**MBTI:** INTJ"));
        assert!(prompt.contains("**Moral Compass:**"));
        assert!(prompt.contains("- Be helpful"));

        assert!(prompt.contains("## Communication Style"));
        assert!(prompt.contains("**Style:** concise"));
        assert!(prompt.contains("**Formality Level:** casual"));

        // Should NOT contain OpenClaw bootstrap file headers
        assert!(!prompt.contains("### SOUL.md"));
        assert!(!prompt.contains("### IDENTITY.md"));
        assert!(!prompt.contains("[File not found"));
    }

    #[test]
    fn aieos_identity_from_inline() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: Some(r#"{"identity":{"names":{"first":"Claw"}}}"#.into()),
        };

        let prompt = build_system_prompt(
            std::env::temp_dir().as_path(),
            "model",
            &[],
            &[],
            Some(&config),
            None,
        );

        assert!(prompt.contains("**Name:** Claw"));
        assert!(prompt.contains("## Identity"));
    }

    #[test]
    fn aieos_fallback_to_openclaw_on_parse_error() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: Some("nonexistent.json".into()),
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should fall back to OpenClaw format when AIEOS file is not found
        // (Error is logged to stderr with filename, not included in prompt)
        assert!(prompt.contains("### SOUL.md"));
    }

    #[test]
    fn aieos_empty_uses_openclaw() {
        use crate::config::IdentityConfig;

        // Format is "aieos" but neither path nor inline is set
        let config = IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should use OpenClaw format (not configured for AIEOS)
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
    }

    #[test]
    fn openclaw_format_uses_bootstrap_files() {
        use crate::config::IdentityConfig;

        let config = IdentityConfig {
            format: "openclaw".into(),
            aieos_path: Some("identity.json".into()),
            aieos_inline: None,
        };

        let ws = make_workspace();
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

        // Should use OpenClaw format even if aieos_path is set
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
        assert!(!prompt.contains("## Identity"));
    }

    #[test]
    fn none_identity_config_uses_openclaw() {
        let ws = make_workspace();
        // Pass None for identity config
        let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

        // Should use OpenClaw format
        assert!(prompt.contains("### SOUL.md"));
        assert!(prompt.contains("Be helpful"));
    }

    #[test]
    fn classify_health_ok_true() {
        let state = classify_health_result(&Ok(true));
        assert_eq!(state, ChannelHealthState::Healthy);
    }

    #[test]
    fn classify_health_ok_false() {
        let state = classify_health_result(&Ok(false));
        assert_eq!(state, ChannelHealthState::Unhealthy);
    }

    #[tokio::test]
    async fn classify_health_timeout() {
        let result = tokio::time::timeout(Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            true
        })
        .await;
        let state = classify_health_result(&result);
        assert_eq!(state, ChannelHealthState::Timeout);
    }

    struct AlwaysFailChannel {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Channel for AlwaysFailChannel {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        ) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("listen boom")
        }
    }

    #[tokio::test]
    async fn supervised_listener_marks_error_and_restarts_on_failures() {
        let calls = Arc::new(AtomicUsize::new(0));
        let channel: Arc<dyn Channel> = Arc::new(AlwaysFailChannel {
            name: "test-supervised-fail",
            calls: Arc::clone(&calls),
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
        let handle = spawn_supervised_listener(channel, tx, 1, 1);

        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(rx);
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["channel:test-supervised-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("listen boom"));
        assert!(calls.load(Ordering::SeqCst) >= 1);
    }
}
