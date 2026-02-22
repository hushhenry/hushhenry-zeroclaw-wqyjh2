//! Struct-based agent loop: Agent owns message_rx and (in session mode) history.
//! Outbound messages are sent via dispatch_outbound_message.

use crate::agent::command::{short_session_id, SlashCommand};
use crate::agent::loop_::{
    build_assistant_content_native, bind_custom_tool_args, find_tool,
    maybe_truncate_tool_output, scrub_credentials, tools_to_specs, MAX_TOOL_ITERATIONS,
};
use crate::channels::traits;
use crate::channels::{
    build_session_turn_history_with_tail, normalize_tail_messages,
    dispatch_outbound_message, outbound_key_from_parts,
    parse_outbound_key_to_delivery_parts, INTERNAL_MESSAGE_CHANNEL, SendMessage,
    ChannelRuntimeContext,
};
use crate::observability::ObserverEvent;
use crate::providers::{ChatMessage, ChatRequest, ToolCall};
use crate::session::compaction::{
    compact_in_memory_history, estimate_tokens, load_compaction_state,
    resolve_keep_recent_messages, SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS,
};
use crate::session::{SessionId, SessionStore};
use crate::tools::ToolSpec;
use anyhow::Result;
use parking_lot::Mutex as ParkingMutex;
use std::collections::HashMap;
use std::convert::AsRef;
use std::fmt::Write;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use uuid::Uuid;

// --- Moved types (from channels/mod.rs) ---

/// One unit of work for an agent's internal queue (user message only).
/// Delivery is via a single outbound_key (internal:{session_id} or channel:{name}:{reply_target}).
#[derive(Clone)]
pub(crate) struct AgentWorkItem {
    pub content: String,
    /// Where to send replies. None when internal message has no resolved target yet (channels set it from session store).
    pub outbound_key: Option<String>,
    pub sender: String,
    pub thread_ts: Option<String>,
}

impl AgentWorkItem {
    pub fn from_message(msg: &traits::ChannelMessage) -> Self {
        let outbound_key = if msg.channel == INTERNAL_MESSAGE_CHANNEL && msg.reply_target.is_empty()
        {
            None
        } else {
            Some(outbound_key_from_parts(&msg.channel, &msg.reply_target))
        };
        Self {
            content: msg.content.clone(),
            outbound_key,
            sender: msg.sender.clone(),
            thread_ts: msg.thread_ts.clone(),
        }
    }

    pub fn to_channel_message(&self) -> traits::ChannelMessage {
        let (channel, reply_target) = self
            .outbound_key
            .as_deref()
            .and_then(|k| parse_outbound_key_to_delivery_parts(k).ok())
            .unwrap_or_default();
        traits::ChannelMessage {
            id: Uuid::new_v4().to_string(),
            sender: self.sender.clone(),
            reply_target,
            content: self.content.clone(),
            channel,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: self.thread_ts.clone(),
            session_id: None,
        }
    }
}

/// Item in the agent queue: either a user message or a slash command to execute in the agent loop.
#[derive(Clone)]
pub(crate) enum AgentQueueItem {
    Message(AgentWorkItem),
    Command(SlashCommand, AgentWorkItem),
}

impl AgentQueueItem {
    /// Delivery envelope (outbound_key) for sending the response.
    pub(crate) fn envelope(&self) -> &AgentWorkItem {
        match self {
            AgentQueueItem::Message(w) => w,
            AgentQueueItem::Command(_, w) => w,
        }
    }
}

pub(crate) struct AgentHandle {
    pub tx: mpsc::Sender<AgentQueueItem>,
}

fn agent_registry() -> Arc<ParkingMutex<HashMap<String, AgentHandle>>> {
    static REGISTRY: LazyLock<Arc<ParkingMutex<HashMap<String, AgentHandle>>>> =
        LazyLock::new(|| Arc::new(ParkingMutex::new(HashMap::new())));
    REGISTRY.clone()
}

/// Drain receiver without blocking.
pub(crate) fn drain_agent_queue(rx: &mut mpsc::Receiver<AgentQueueItem>) -> Vec<AgentQueueItem> {
    let mut batch: Vec<AgentQueueItem> = Vec::new();
    while let Ok(w) = rx.try_recv() {
        batch.push(w);
    }
    batch
}

/// Split drained items into commands (first, in order) and messages (second). Used so commands are
/// processed and delivered before merging messages (e.g. steer-merge).
pub(crate) fn partition_steer_items(
    items: Vec<AgentQueueItem>,
) -> (Vec<(SlashCommand, AgentWorkItem)>, Vec<AgentWorkItem>) {
    let mut commands = Vec::new();
    let mut messages = Vec::new();
    for item in items {
        match item {
            AgentQueueItem::Message(w) => messages.push(w),
            AgentQueueItem::Command(cmd, w) => commands.push((cmd, w)),
        }
    }
    (commands, messages)
}

pub(crate) fn merge_work_items(mut batch: Vec<AgentWorkItem>) -> Option<AgentWorkItem> {
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

pub(crate) fn build_steer_merge_message(
    current_user_content: &str,
    pending: Vec<AgentWorkItem>,
) -> String {
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

/// Agent registry key: always the session_id.
pub(crate) fn agent_registry_key(session_id: &SessionId, _inbound_key: &str) -> String {
    session_id.as_str().to_string()
}

// --- Agent struct ---

pub(crate) struct Agent {
    session_id: SessionId,
    inbound_key: String,
    system_prompt: String,
    tool_allow_list: Option<Vec<String>>,
    /// Tool specs for chat requests; computed once in new() from tools_registry + tool_allow_list.
    tool_specs: Vec<ToolSpec>,
    message_rx: mpsc::Receiver<AgentQueueItem>,
    history: Vec<ChatMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    /// When true, tool output is also sent outbound (verbose).
    verbose_tool_output: bool,
}

impl Agent {
    /// Initialize agent: resolve prompt/tools. Caller creates (message_tx, message_rx); we take message_rx.
    pub(crate) fn new(
        ctx: Arc<ChannelRuntimeContext>,
        session_id: SessionId,
        inbound_key: &str,
        message_rx: mpsc::Receiver<AgentQueueItem>,
        verbose_tool_output: bool,
    ) -> Result<Self> {
        let (system_prompt, tool_allow_list) =
            crate::channels::resolve_effective_system_prompt_and_tool_allow_list(
                &ctx,
                Some(&session_id),
                "", // channel name resolved per-turn in session_context_loop
            );
        let tool_specs = tools_to_specs(ctx.tools_registry.as_ref(), tool_allow_list.as_deref());

        let history = if let Some(store) = ctx.session_store.as_ref() {
            let compaction_state = load_compaction_state(store, &session_id).unwrap_or_default();
            let tail_messages = store
                .load_messages_after_id(&session_id, compaction_state.after_message_id)
                .unwrap_or_default();
            let (tail_chat, _) = normalize_tail_messages(&tail_messages);
            build_session_turn_history_with_tail(&system_prompt, &tail_chat, None)
        } else {
            vec![]
        };

        Ok(Self {
            session_id: session_id.clone(),
            inbound_key: inbound_key.to_string(),
            system_prompt,
            tool_allow_list,
            tool_specs,
            message_rx,
            history,
            ctx,
            verbose_tool_output,
        })
    }

    /// Entry point: run session context loop. Registry entry for this key is removed here when the loop returns.
    pub(crate) async fn run(
        self,
        registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        key_str: String,
    ) {
        self.session_context_loop(Arc::clone(&registry), key_str.clone())
            .await;
        registry.lock().remove(&key_str);
    }

    /// Send one outbound message via dispatch_outbound_message. Skips when is_tool && !verbose_tool_output.
    /// When outbound_key is None, uses the session's outbound_key from the session store if available.
    async fn send_outbound(
        &self,
        content: &str,
        is_tool: bool,
        outbound_key: Option<&str>,
    ) -> Result<()> {
        if is_tool && !self.verbose_tool_output {
            return Ok(());
        }
        let key = outbound_key
            .map(String::from)
            .or_else(|| {
                self.ctx.session_store.as_ref().and_then(|store| {
                    store
                        .get_outbound_key_for_session(self.session_id.as_str())
                        .ok()
                        .flatten()
                })
            });
        let Some(ref key) = key else {
            tracing::debug!("send_outbound: no outbound_key (work item or session), skipping");
            return Ok(());
        };
        let (_, reply_target) = parse_outbound_key_to_delivery_parts(key)
            .map_err(|e| anyhow::anyhow!("invalid outbound_key: {e}"))?;
        let msg = SendMessage::new(content, reply_target);
        dispatch_outbound_message(key.as_str(), msg).await.map_err(Into::into)
    }

    /// /stop: abort current turn. Delivers "Agent was aborted.", returns true.
    async fn cmd_stop(&mut self, envelope: &AgentWorkItem) -> Result<bool> {
        self.send_outbound("Agent was aborted.", false, envelope.outbound_key.as_deref())
            .await?;
        Ok(true)
    }

    /// /new: create new session, deliver "New session started · model: <provider>/<model>".
    async fn cmd_new(&mut self, envelope: &AgentWorkItem) -> Result<bool> {
        let store = self
            .ctx
            .session_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session store is unavailable"))?;
        match store.create_new(&self.inbound_key) {
            Ok(new_session_id) => {
                let full_model = self.ctx.provider_manager.default_full_model().to_string();
                self.send_outbound(
                    &format!("New session started · model: {full_model}"),
                    false,
                    envelope.outbound_key.as_deref(),
                )
                .await?;
            }
            Err(e) => {
                tracing::error!(
                    "Failed to create new session for key {}: {e}",
                    self.inbound_key.as_str()
                );
                self.send_outbound(
                    "⚠️ Failed to create a new session. Please try again.",
                    false,
                    envelope.outbound_key.as_deref(),
                )
                .await?;
            }
        }
        Ok(false)
    }

    /// /compact: compact this agent's in-memory history, then deliver confirmation.
    async fn cmd_compact(&mut self, envelope: &AgentWorkItem) -> Result<bool> {
        let store = Arc::clone(
            self.ctx
                .session_store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Session store is unavailable"))?,
        );
        let session_id = self.session_id.clone();
        let compacted = self
            .run_compact_in_memory(&session_id, store.as_ref())
            .await?;
        let msg = if compacted {
            format!(
                "Session compacted successfully for `{}`.",
                short_session_id(&session_id)
            )
        } else {
            "No compaction needed yet. Session tail is already small.".to_string()
        };
        self.send_outbound(&msg, false, envelope.outbound_key.as_deref())
            .await?;
        Ok(false)
    }

    /// /model (no args): show current model from session override or default (uses agent's ctx + session_id).
    async fn cmd_model_show(&mut self, envelope: &AgentWorkItem) -> Result<bool> {
        let current = self
            .ctx
            .session_store
            .as_ref()
            .and_then(|store| {
                store
                    .get_state_key(&self.session_id, SessionStore::MODEL_OVERRIDE_KEY)
                    .ok()
                    .flatten()
            })
            .and_then(|raw| crate::channels::decode_session_string_state(Some(raw)))
            .unwrap_or_else(|| self.ctx.provider_manager.default_full_model().to_string());
        self.send_outbound(&current, false, envelope.outbound_key.as_deref())
            .await?;
        Ok(false)
    }

    /// /model <provider>/<model>: set session model override.
    async fn cmd_model_set(
        &mut self,
        provider_model: &str,
        envelope: &AgentWorkItem,
    ) -> Result<bool> {
        let store = self
            .ctx
            .session_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session store is unavailable"))?;
        let value_json = serde_json::to_string(&provider_model.trim())
            .unwrap_or_else(|_| format!("\"{}\"", provider_model.trim()));
        if let Err(e) = store.set_state_key(
            &self.session_id,
            SessionStore::MODEL_OVERRIDE_KEY,
            &value_json,
        ) {
            tracing::error!("Failed to set model_override: {e}");
            self.send_outbound(
                "⚠️ Failed to set model override.",
                false,
                envelope.outbound_key.as_deref(),
            )
            .await?;
            return Ok(false);
        }
        self.send_outbound(
            &format!(
                "Model override set to `{}` for this session.",
                provider_model.trim()
            ),
            false,
            envelope.outbound_key.as_deref(),
        )
        .await?;
        Ok(false)
    }

    /// /models: show model info for this session (override + hint).
    async fn cmd_models(&mut self, envelope: &AgentWorkItem) -> Result<bool> {
        let store = self
            .ctx
            .session_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session store is unavailable"))?;
        let override_val = crate::channels::decode_session_string_state(
            store
                .get_state_key(&self.session_id, SessionStore::MODEL_OVERRIDE_KEY)
                .ok()
                .flatten(),
        );
        let mut text = String::new();
        let _ = writeln!(
            text,
            "Model for session `{}`",
            short_session_id(&self.session_id)
        );
        let _ = writeln!(
            text,
            "Override: {}",
            override_val
                .as_deref()
                .unwrap_or("(none — use agent/default)")
        );
        let _ = writeln!(
            text,
            "Use /model <provider>/<model> to set override (e.g. openrouter/anthropic/claude-sonnet-4)."
        );
        self.send_outbound(text.trim(), false, envelope.outbound_key.as_deref())
            .await?;
        Ok(false)
    }

    /// Execute one agent command using agent state; deliver reply. Returns true if /stop (abort turn).
    async fn run_command_and_deliver(
        &mut self,
        command: SlashCommand,
        envelope: &AgentWorkItem,
    ) -> Result<bool> {
        match command {
            SlashCommand::Stop => self.cmd_stop(envelope).await,
            SlashCommand::New => self.cmd_new(envelope).await,
            SlashCommand::Compact => self.cmd_compact(envelope).await,
            SlashCommand::Models => self.cmd_models(envelope).await,
            SlashCommand::Model { provider_model } => {
                if provider_model.is_empty() {
                    self.cmd_model_show(envelope).await
                } else {
                    self.cmd_model_set(provider_model.as_str(), envelope).await
                }
            }
            _ => Err(anyhow::anyhow!(
                "run_command_and_deliver called with non-agent command"
            )),
        }
    }

    /// Non-blocking drain of message_rx for steer-merge.
    /// Session context loop: process items in order. /stop aborts only messages that appeared
    /// before it in the batch; messages after /stop are executed normally.
    async fn session_context_loop(
        mut self,
        _registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        _key_str: String,
    ) {
        let session_id = self.session_id.clone();
        let session_store = self.ctx.session_store.as_ref().map(Arc::clone);

        while let Some(first) = self.message_rx.recv().await {
            let mut batch = vec![first];
            batch.extend(drain_agent_queue(&mut self.message_rx));

            let mut message_buf: Vec<AgentWorkItem> = Vec::new();
            for item in batch {
                match item {
                    AgentQueueItem::Message(w) => message_buf.push(w),
                    AgentQueueItem::Command(cmd, envelope) => {
                        match self.run_command_and_deliver(cmd, &envelope).await {
                            Ok(true) => {
                                // /stop: discard only messages before this command
                                message_buf.clear();
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::error!(session_id = %session_id.as_str(), "Command error: {e}");
                            }
                        }
                    }
                }
            }

            let Some(work) = merge_work_items(message_buf) else {
                continue;
            };

            match self
                .tool_call_loop_session(&session_id, session_store.as_deref(), &work)
                .await
            {
                Ok(Some((user_content, assistant_content))) => {
                    if let Some(store) = session_store.as_ref() {
                        let _ = store.append_message(&session_id, "user", &user_content, None);
                        let _ = store.append_message(
                            &session_id,
                            "assistant",
                            &assistant_content,
                            None,
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(session_id = %session_id.as_str(), "Agent turn error: {e}");
                }
            }
        }
    }

    /// Tool call loop for session mode: no DB read after init; trailing orphan user merged with new user.
    /// Returns Ok(Some((user, assistant))) on successful turn (persist both after return); Ok(None) on abort (e.g. /stop).
    async fn tool_call_loop_session(
        &mut self,
        session_id: &SessionId,
        session_store: Option<&SessionStore>,
        work: &AgentWorkItem,
    ) -> Result<Option<(String, String)>> {
        let delivery_outbound_key = work.outbound_key.clone();

        let new_content = work.content.clone();
        let (user_content, _, _) = {
            let last_is_user = self
                .history
                .last()
                .map(|m| m.role == "user")
                .unwrap_or(false);
            if last_is_user {
                let last = self.history.last_mut().expect("non-empty");
                let merged = format!("{}\n\n{}", last.content.trim(), new_content.trim());
                last.content = merged.clone();
                (merged, new_content, true)
            } else {
                self.history.push(ChatMessage::user(new_content.clone()));
                (new_content.clone(), new_content, false)
            }
        };

        if let Some(store) = session_store {
            if estimate_tokens(&self.history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS {
                self.run_compact_in_memory(session_id, store).await?;
            }
        }

        let provider_ctx =
            crate::channels::resolve_turn_provider_model_temperature(&self.ctx, Some(session_id));

        let provider_name = "agent";
        for _iter in 0..MAX_TOOL_ITERATIONS {
            self.ctx.observer.record_event(&ObserverEvent::LlmRequest {
                provider: provider_name.to_string(),
                model: provider_ctx.model.clone(),
                messages_count: self.history.len(),
            });

            let resp = match provider_ctx
                .provider
                .chat(
                    ChatRequest {
                        messages: self.history.as_slice(),
                        tools: self.tool_slice(),
                    },
                    &provider_ctx.model,
                    provider_ctx.temperature,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let err_str = e.to_string();
                    if is_context_exceeded_error(&err_str) {
                        if let Some(store) = session_store {
                            self.run_compact_in_memory(session_id, store).await?;
                        }
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let text = resp.text_or_empty().to_string();
            let tool_calls = resp.tool_calls;

            if tool_calls.is_empty() {
                self.history.push(ChatMessage::assistant(text.clone()));
                self.send_outbound(&text, false, delivery_outbound_key.as_deref()).await?;
                let last_user_content = self
                    .history
                    .iter()
                    .rev()
                    .find(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                return Ok(Some((last_user_content, text)));
            }

            let assistant_content = build_assistant_content_native(&text, &tool_calls);
            self.send_outbound(&text, false, delivery_outbound_key.as_deref()).await?;
            self.history.push(ChatMessage::assistant(assistant_content));

            for (idx, call) in tool_calls.iter().enumerate() {
                let steer_items = drain_agent_queue(&mut self.message_rx);
                let (steer_commands, steer_messages) = partition_steer_items(steer_items);

                for (cmd, envelope) in steer_commands {
                    match self.run_command_and_deliver(cmd, &envelope).await {
                        Ok(true) => return Ok(None),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(session_id = %session_id.as_str(), "Command error: {e}");
                        }
                    }
                }

                if !steer_messages.is_empty() {
                    let skip_msg = "tool skipped (new user message)";
                    for c in &tool_calls[idx..] {
                        self.history.push(ChatMessage::tool(
                            serde_json::json!({
                                "tool_call_id": c.id,
                                "content": skip_msg
                            })
                            .to_string(),
                        ));
                        self.send_outbound(skip_msg, true, delivery_outbound_key.as_deref())
                            .await?;
                    }
                    let merged = build_steer_merge_message(&user_content, steer_messages);
                    self.history.push(ChatMessage::user(merged));
                    break;
                }

                let args = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                let tool_args =
                    bind_custom_tool_args(&call.name, args, Some(session_id.as_str()));
                let (output, _success) = self.execute_single_tool(&call, tool_args).await;
                let output = maybe_truncate_tool_output(&output);
                self.history.push(ChatMessage::tool(
                    serde_json::json!({
                        "tool_call_id": call.id,
                        "content": output
                    })
                    .to_string(),
                ));
                self.send_outbound(&output, true, delivery_outbound_key.as_deref())
                    .await?;
            }
        }

        anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
    }

    /// Compact in-memory history using agent state. Returns true if compaction was performed.
    async fn run_compact_in_memory(
        &mut self,
        session_id: &SessionId,
        session_store: &SessionStore,
    ) -> Result<bool> {
        let keep_recent = resolve_keep_recent_messages(self.ctx.session_history_limit);
        let default_resolved = self
            .ctx
            .provider_manager
            .default_resolved()
            .map_err(anyhow::Error::msg)?;
        match compact_in_memory_history(
            &self.history,
            session_store,
            session_id,
            &default_resolved,
            &self.system_prompt,
            keep_recent,
        )
        .await
        {
            Ok((new_history, compacted)) => {
                self.history = new_history;
                Ok(compacted)
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id.as_str(), "In-memory compaction failed: {e}");
                Ok(false)
            }
        }
    }

    async fn execute_single_tool(
        &self,
        call: &ToolCall,
        tool_args: serde_json::Value,
    ) -> (String, bool) {
        let tool_allow_list = self.tool_allow_list.as_deref();
        let disallowed =
            tool_allow_list.map(|allow| !allow.iter().any(|n| n.as_str() == call.name));
        if disallowed == Some(true) {
            return (
                format!(
                    "Tool '{}' is not available for this agent (policy restriction).",
                    call.name
                ),
                false,
            );
        }
        let Some(tool) = find_tool(self.ctx.tools_registry.as_ref(), &call.name) else {
            return (format!("Unknown tool: {}", call.name), false);
        };
        match tool.execute(tool_args).await {
            Ok(r) => {
                if r.success {
                    (scrub_credentials(&r.output), true)
                } else {
                    (
                        format!("Error: {}", r.error.unwrap_or_else(|| r.output)),
                        false,
                    )
                }
            }
            Err(e) => (format!("Error executing {}: {e}", call.name), false),
        }
    }

    /// Slice of tool specs for chat requests; None if empty (avoids per-turn allocation).
    fn tool_slice(&self) -> Option<&[ToolSpec]> {
        if self.tool_specs.is_empty() {
            None
        } else {
            Some(self.tool_specs.as_slice())
        }
    }
}

fn is_context_exceeded_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("context")
        && (lower.contains("length") || lower.contains("token") || lower.contains("exceeded"))
}

/// Returns a sender to the agent's queue. Spawns Agent::run.
pub(crate) fn get_or_create_agent(
    ctx: Arc<ChannelRuntimeContext>,
    session_id: SessionId,
    inbound_key: &str,
) -> mpsc::Sender<AgentQueueItem> {
    let key_str = agent_registry_key(&session_id, inbound_key);
    let registry = agent_registry();
    {
        let guard = registry.lock();
        if let Some(handle) = guard.get(&key_str) {
            return handle.tx.clone();
        }
    }
    let (message_tx, message_rx) = mpsc::channel::<AgentQueueItem>(64);

    match Agent::new(
        Arc::clone(&ctx),
        session_id,
        inbound_key,
        message_rx,
        false,
    ) {
        Ok(agent) => {
            let reg = agent_registry();
            let key_str2 = key_str.clone();
            tokio::spawn(async move {
                agent.run(reg, key_str2).await;
            });
            registry.lock().insert(
                key_str,
                AgentHandle {
                    tx: message_tx.clone(),
                },
            );
        }
        Err(e) => {
            tracing::error!("Agent::new failed: {e}");
        }
    }

    message_tx
}
