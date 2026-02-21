//! Struct-based agent loop: Agent owns message_rx, deliver_tx, and (in session mode) history.
//! Replaces the function-based agent_loop in channels/mod.rs.

use crate::agent::command::build_route_metadata;
use crate::agent::loop_::{
    build_assistant_content_native, find_tool, maybe_bind_source_session_id,
    maybe_truncate_tool_output, scrub_credentials, tools_to_specs, MAX_TOOL_ITERATIONS,
};
use crate::channels::traits;
use crate::channels::{
    build_ephemeral_announce_context, build_session_turn_history, send_delivery_message,
    should_deliver_to_external_channel, ChannelRuntimeContext, INTERNAL_MESSAGE_CHANNEL,
};
use crate::observability::{Observer, ObserverEvent};
use crate::providers::{ChatMessage, ChatRequest, ProviderCtx, ToolCall};
use crate::session::compaction::{
    estimate_tokens, load_compaction_state, maybe_compact, resolve_keep_recent_messages,
    CompactionState, SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS,
};
use crate::session::{SessionId, SessionKey, SessionStore};
use crate::tools::Tool;
use anyhow::Result;
use parking_lot::Mutex as ParkingMutex;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use uuid::Uuid;

// --- Moved types (from channels/mod.rs) ---

/// One unit of work for an agent's internal queue.
#[derive(Clone)]
pub(crate) struct AgentWorkItem {
    pub content: String,
    pub reply_target: String,
    pub channel: String,
    pub sender: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub agent_id: Option<String>,
    pub account_id: Option<String>,
    pub title: Option<String>,
    pub chat_type: traits::ChatType,
    pub raw_chat_type: Option<String>,
}

impl AgentWorkItem {
    pub fn from_message(msg: &traits::ChannelMessage) -> Self {
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

    pub fn to_channel_message(&self) -> traits::ChannelMessage {
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

pub(crate) struct AgentHandle {
    pub tx: mpsc::Sender<AgentWorkItem>,
}

fn agent_registry() -> Arc<ParkingMutex<HashMap<String, AgentHandle>>> {
    static REGISTRY: LazyLock<Arc<ParkingMutex<HashMap<String, AgentHandle>>>> =
        LazyLock::new(|| Arc::new(ParkingMutex::new(HashMap::new())));
    REGISTRY.clone()
}

/// Drain receiver without blocking.
pub(crate) fn drain_agent_queue(rx: &mut mpsc::Receiver<AgentWorkItem>) -> Vec<AgentWorkItem> {
    let mut batch: Vec<AgentWorkItem> = Vec::new();
    while let Ok(w) = rx.try_recv() {
        batch.push(w);
    }
    batch
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

// --- Deliver channel ---

#[derive(Clone)]
pub(crate) enum DeliverMessage {
    Assistant {
        content: String,
        channel_name: String,
        reply_target: String,
    },
    Tool {
        tool_name: String,
        content: String,
        channel_name: String,
        reply_target: String,
    },
}

/// Agent key: session_id when session mode, or "memory:{session_key}" when memory mode.
pub(crate) fn agent_registry_key(
    session_id: Option<&SessionId>,
    session_key: &SessionKey,
) -> String {
    session_id
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| format!("memory:{}", session_key.as_str()))
}

// --- Agent struct ---

pub(crate) struct Agent {
    session_id: Option<SessionId>,
    system_prompt: String,
    tool_allow_list: Option<Vec<String>>,
    message_rx: mpsc::Receiver<AgentWorkItem>,
    deliver_tx: mpsc::Sender<DeliverMessage>,
    history: Vec<ChatMessage>,
    use_session_history: bool,
    ctx: Arc<ChannelRuntimeContext>,
    /// When true, deliver_loop also sends Tool variants (verbose).
    verbose_tool_output: bool,
}

impl Agent {
    /// Initialize agent: resolve prompt/tools, create deliver channel, spawn deliver_loop.
    /// Caller creates (message_tx, message_rx) and (deliver_tx, deliver_rx); we take message_rx and deliver_tx.
    pub(crate) fn new(
        ctx: Arc<ChannelRuntimeContext>,
        session_id: Option<SessionId>,
        session_key: &SessionKey,
        use_session_history: bool,
        message_rx: mpsc::Receiver<AgentWorkItem>,
        deliver_tx: mpsc::Sender<DeliverMessage>,
        verbose_tool_output: bool,
    ) -> Result<Self> {
        let (system_prompt, tool_allow_list) =
            crate::channels::resolve_effective_system_prompt_and_tool_allow_list(
                &ctx,
                session_id.as_ref(),
                "", // channel name resolved per-turn in session_context_loop
            );

        let history = if use_session_history {
            if let (Some(store), Some(ref sid)) = (ctx.session_store.as_ref(), &session_id) {
                let compaction_state = load_compaction_state(store, sid).unwrap_or_default();
                let _tail = store
                    .load_messages_after_id(sid, compaction_state.after_message_id)
                    .unwrap_or_default();
                // History built per-turn with user content in session_context_loop.
                vec![]
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        Ok(Self {
            session_id,
            system_prompt,
            tool_allow_list,
            message_rx,
            deliver_tx,
            history,
            use_session_history,
            ctx,
            verbose_tool_output,
        })
    }

    /// Entry point: run session_context_loop or memory_context_loop.
    pub(crate) async fn run(
        mut self,
        registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        key_str: String,
    ) {
        if self.use_session_history {
            let sid = match self.session_id.take() {
                Some(s) => s,
                None => {
                    tracing::error!(key = %key_str, "session_context_loop requires session_id");
                    registry.lock().remove(&key_str);
                    return;
                }
            };
            self.session_context_loop(registry, key_str, sid).await;
        } else {
            self.memory_context_loop(registry, key_str).await;
        }
    }

    /// Spawned task: consumes DeliverMessage, routes assistant (and optionally tool) to send_delivery_message.
    pub(crate) async fn deliver_loop(
        ctx: Arc<ChannelRuntimeContext>,
        mut rx: mpsc::Receiver<DeliverMessage>,
        verbose_tool_output: bool,
    ) {
        while let Some(msg) = rx.recv().await {
            let (content, channel_name, reply_target, is_tool) = match &msg {
                DeliverMessage::Assistant {
                    content,
                    channel_name,
                    reply_target,
                } => (
                    content.as_str(),
                    channel_name.as_str(),
                    reply_target.as_str(),
                    false,
                ),
                DeliverMessage::Tool {
                    content,
                    channel_name,
                    reply_target,
                    ..
                } => (
                    content.as_str(),
                    channel_name.as_str(),
                    reply_target.as_str(),
                    true,
                ),
            };
            if is_tool && !verbose_tool_output {
                continue;
            }
            let target = ctx.channels_by_name.get(channel_name);
            if let Err(e) = send_delivery_message(target, channel_name, reply_target, content).await
            {
                tracing::debug!("deliver_loop: failed to send: {e}");
            }
        }
    }

    fn on_message(
        &mut self,
        content: &str,
        is_tool: bool,
        channel_name: &str,
        reply_target: &str,
    ) -> Result<()> {
        let msg = if is_tool {
            DeliverMessage::Tool {
                tool_name: String::new(),
                content: content.to_string(),
                channel_name: channel_name.to_string(),
                reply_target: reply_target.to_string(),
            }
        } else {
            DeliverMessage::Assistant {
                content: content.to_string(),
                channel_name: channel_name.to_string(),
                reply_target: reply_target.to_string(),
            }
        };
        let _ = self.deliver_tx.try_send(msg);
        Ok(())
    }

    /// Non-blocking drain of message_rx for steer-merge.
    /// Session context loop: sequential, in-memory history, steer-merge.
    async fn session_context_loop(
        mut self,
        registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        key_str: String,
        session_id: SessionId,
    ) {
        let session_store = match self.ctx.session_store.as_ref() {
            Some(s) => Arc::clone(s),
            None => {
                registry.lock().remove(&key_str);
                return;
            }
        };

        while let Some(first) = self.message_rx.recv().await {
            let mut batch = vec![first];
            batch.extend(drain_agent_queue(&mut self.message_rx));
            let Some(work) = merge_work_items(batch) else {
                continue;
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

            let delivery_channel_name = resolve_delivery_target(&self.ctx, &session_id, &msg).0;
            let delivery_reply_target = resolve_delivery_target(&self.ctx, &session_id, &msg).1;

            if let Err(e) = self
                .tool_call_loop_session(
                    &session_id,
                    &session_store,
                    &msg,
                    &delivery_channel_name,
                    &delivery_reply_target,
                )
                .await
            {
                tracing::error!(session_id = %session_id.as_str(), "Agent turn error: {e}");
            }
        }
        registry.lock().remove(&key_str);
    }

    /// Tool call loop for session mode: on_message, mid-tool steer, context_exceeded retry.
    #[allow(clippy::too_many_arguments)]
    async fn tool_call_loop_session(
        &mut self,
        session_id: &SessionId,
        session_store: &SessionStore,
        msg: &traits::ChannelMessage,
        delivery_channel_name: &str,
        delivery_reply_target: &str,
    ) -> Result<()> {
        let compaction_state = load_compaction_state(session_store, session_id)?;
        let tail_messages = session_store
            .load_messages_after_id(session_id, compaction_state.after_message_id)
            .unwrap_or_default();
        let ephemeral = build_ephemeral_announce_context(&tail_messages);
        let user_content = if ephemeral.is_empty() {
            msg.content.clone()
        } else {
            format!("{ephemeral}\n\n{}", msg.content)
        };

        self.history = build_session_turn_history(
            &self.system_prompt,
            &compaction_state,
            &tail_messages,
            &user_content,
        );

        if estimate_tokens(&self.history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS {
            self.compact_history(session_id, session_store, delivery_channel_name)
                .await?;
            let compaction_state = load_compaction_state(session_store, session_id)?;
            let tail_messages = session_store
                .load_messages_after_id(session_id, compaction_state.after_message_id)
                .unwrap_or_default();
            self.history = build_session_turn_history(
                &self.system_prompt,
                &compaction_state,
                &tail_messages,
                &user_content,
            );
        }

        let provider_ctx =
            crate::channels::resolve_turn_provider_model_temperature(&self.ctx, Some(session_id));
        let tool_specs = tools_to_specs(
            self.ctx.tools_registry.as_ref(),
            self.tool_allow_list.as_deref(),
        );
        let tool_slice = if tool_specs.is_empty() {
            None
        } else {
            Some(tool_specs.as_slice())
        };

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
                        tools: tool_slice,
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
                        self.compact_history(session_id, session_store, delivery_channel_name)
                            .await?;
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let text = resp.text_or_empty().to_string();
            let tool_calls = resp.tool_calls;

            if tool_calls.is_empty() {
                self.history.push(ChatMessage::assistant(text.clone()));
                self.on_message(&text, false, delivery_channel_name, delivery_reply_target)?;
                if should_deliver_to_external_channel(
                    self.ctx.session_store.as_ref(),
                    Some(session_id),
                ) {
                    let _ = session_store.append_message(session_id, "user", &user_content, None);
                    let _ = session_store.append_message(session_id, "assistant", &text, None);
                }
                return Ok(());
            }

            let assistant_content = build_assistant_content_native(&text, &tool_calls);
            self.on_message(&text, false, delivery_channel_name, delivery_reply_target)?;
            self.history.push(ChatMessage::assistant(assistant_content));

            for (idx, call) in tool_calls.iter().enumerate() {
                let steer = drain_agent_queue(&mut self.message_rx);
                if !steer.is_empty() {
                    let skip_msg = "tool skipped (new user message)";
                    for c in &tool_calls[idx..] {
                        self.history.push(ChatMessage::tool(
                            serde_json::json!({
                                "tool_call_id": c.id,
                                "content": skip_msg
                            })
                            .to_string(),
                        ));
                        self.on_message(
                            skip_msg,
                            true,
                            delivery_channel_name,
                            delivery_reply_target,
                        )?;
                    }
                    let merged = build_steer_merge_message(&user_content, steer);
                    self.history.push(ChatMessage::user(merged));
                    // Continue the for _iter loop to make another LLM call with merged context.
                    break;
                }

                let args = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                let tool_args =
                    maybe_bind_source_session_id(&call.name, args, Some(session_id.as_str()));
                let (output, success) = self.execute_single_tool(&call, tool_args).await;
                let output = maybe_truncate_tool_output(&output);
                self.history.push(ChatMessage::tool(
                    serde_json::json!({
                        "tool_call_id": call.id,
                        "content": output
                    })
                    .to_string(),
                ));
                self.on_message(&output, true, delivery_channel_name, delivery_reply_target)?;
            }
        }

        anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
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

    async fn compact_history(
        &mut self,
        session_id: &SessionId,
        session_store: &SessionStore,
        channel_name: &str,
    ) -> Result<()> {
        let keep_recent = resolve_keep_recent_messages(self.ctx.session_history_limit);
        let default_resolved = self
            .ctx
            .provider_manager
            .default_resolved()
            .map_err(anyhow::Error::msg)?;
        maybe_compact(
            session_store,
            session_id,
            &default_resolved,
            &self.system_prompt,
            keep_recent,
        )
        .await?;
        Ok(())
    }

    /// Memory context loop: spawn one memory_context_turn per message.
    async fn memory_context_loop(
        mut self,
        registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        key_str: String,
    ) {
        while let Some(work) = self.message_rx.recv().await {
            let msg = work.to_channel_message();
            let worker_ctx = Arc::clone(&self.ctx);
            tokio::spawn(async move {
                if let Err(e) = crate::agent::turn::run_memory_turn(worker_ctx, None, msg).await {
                    tracing::error!("Memory turn error: {e}");
                }
            });
        }
        registry.lock().remove(&key_str);
    }
}

fn resolve_delivery_target(
    ctx: &ChannelRuntimeContext,
    session_id: &SessionId,
    msg: &traits::ChannelMessage,
) -> (String, String) {
    let mut ch = msg.channel.clone();
    let mut rt = msg.reply_target.clone();
    if msg.channel == INTERNAL_MESSAGE_CHANNEL {
        if let (Some(store), Some(sid)) = (ctx.session_store.as_ref(), Some(session_id)) {
            if let Ok(Some(meta)) = store.load_route_metadata(sid) {
                if meta.channel != INTERNAL_MESSAGE_CHANNEL {
                    rt = meta.route_id.unwrap_or(meta.chat_id);
                    ch = meta.channel;
                }
            }
        }
    }
    (ch, rt)
}

fn is_context_exceeded_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("context")
        && (lower.contains("length") || lower.contains("token") || lower.contains("exceeded"))
}

/// Returns a sender to the agent's queue. Spawns deliver_loop and Agent::run.
pub(crate) fn get_or_create_agent(
    ctx: Arc<ChannelRuntimeContext>,
    session_id: Option<SessionId>,
    session_key: &SessionKey,
    use_session_history: bool,
) -> mpsc::Sender<AgentWorkItem> {
    let key_str = agent_registry_key(session_id.as_ref(), session_key);
    let registry = agent_registry();
    {
        let guard = registry.lock();
        if let Some(handle) = guard.get(&key_str) {
            return handle.tx.clone();
        }
    }
    let (message_tx, message_rx) = mpsc::channel(64);
    let (deliver_tx, deliver_rx) = mpsc::channel(64);

    let runner_ctx = Arc::clone(&ctx);
    tokio::spawn(async move {
        Agent::deliver_loop(runner_ctx, deliver_rx, false).await;
    });

    match Agent::new(
        Arc::clone(&ctx),
        session_id,
        session_key,
        use_session_history,
        message_rx,
        deliver_tx,
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
