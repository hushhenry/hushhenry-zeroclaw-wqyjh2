//! Struct-based agent loop: Agent owns message_rx, deliver_tx, and (in session mode) history.
//! Replaces the function-based agent_loop in channels/mod.rs.

use crate::agent::command::build_route_metadata;
use crate::agent::loop_::{
    build_assistant_content_native, find_tool, maybe_bind_source_session_id,
    maybe_truncate_tool_output, scrub_credentials, tool_results_to_chat_messages, tools_to_specs,
    ToolExecutionResult, MAX_TOOL_ITERATIONS,
};
use crate::channels::traits;
use crate::channels::{
    build_memory_context, build_session_turn_history_with_tail, conversation_memory_key,
    normalize_tail_messages, resolve_turn_provider_model_temperature, send_delivery_message,
    should_deliver_to_external_channel, ChannelRuntimeContext, CHANNEL_MESSAGE_TIMEOUT_SECS,
    INTERNAL_MESSAGE_CHANNEL,
};
use crate::memory::MemoryCategory;
use crate::observability::ObserverEvent;
use crate::providers::{ChatMessage, ChatRequest, ToolCall};
use crate::session::compaction::{
    compact_in_memory_history, estimate_tokens, load_compaction_state, resolve_keep_recent_messages,
    SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS,
};
use crate::session::{SessionId, SessionKey, SessionStore};
use crate::tools::ToolSpec;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use parking_lot::Mutex as ParkingMutex;
use std::collections::HashMap;
use std::convert::AsRef;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout as timeout_future;
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
    /// Tool specs for chat requests; computed once in new() from tools_registry + tool_allow_list.
    tool_specs: Vec<ToolSpec>,
    message_rx: mpsc::Receiver<AgentWorkItem>,
    deliver_tx: mpsc::Sender<DeliverMessage>,
    history: Vec<ChatMessage>,
    /// DB message id per history entry (Some for persisted user/assistant, None for system/summary/tool).
    history_message_ids: Vec<Option<i64>>,
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
        let tool_specs = tools_to_specs(ctx.tools_registry.as_ref(), tool_allow_list.as_deref());

        let (history, history_message_ids) = if use_session_history {
            if let (Some(store), Some(ref sid)) = (ctx.session_store.as_ref(), &session_id) {
                let compaction_state = load_compaction_state(store, sid).unwrap_or_default();
                let tail_messages = store
                    .load_messages_after_id(sid, compaction_state.after_message_id)
                    .unwrap_or_default();
                let (tail_chat, tail_ids) = normalize_tail_messages(&tail_messages);
                let history = build_session_turn_history_with_tail(
                    &system_prompt,
                    &compaction_state,
                    &tail_chat,
                    None,
                );
                let mut ids = vec![None; history.len().saturating_sub(tail_chat.len())];
                ids.extend(tail_ids);
                (history, ids)
            } else {
                (vec![], vec![])
            }
        } else {
            (vec![], vec![])
        };

        Ok(Self {
            session_id,
            system_prompt,
            tool_allow_list,
            tool_specs,
            message_rx,
            deliver_tx,
            history,
            history_message_ids,
            use_session_history,
            ctx,
            verbose_tool_output,
        })
    }

    /// Entry point: run session_context_loop or memory_context_loop.
    /// Registry entry for this key is removed here when the loop returns (single place).
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
            self.session_context_loop(Arc::clone(&registry), key_str.clone(), sid)
                .await;
        } else {
            self.memory_context_loop(Arc::clone(&registry), key_str.clone())
                .await;
        }
        registry.lock().remove(&key_str);
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
            None => return,
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

            if let Err(e) = self
                .tool_call_loop_session(&session_id, &session_store, &msg)
                .await
            {
                tracing::error!(session_id = %session_id.as_str(), "Agent turn error: {e}");
            }
        }
    }

    /// Tool call loop for session mode: no DB read after init; trailing orphan user merged with new user; persist user at start, assistant at end only.
    async fn tool_call_loop_session(
        &mut self,
        session_id: &SessionId,
        session_store: &SessionStore,
        msg: &traits::ChannelMessage,
    ) -> Result<()> {
        let (delivery_channel_name, delivery_reply_target) =
            resolve_delivery_target(&self.ctx, Some(session_id), msg);

        let new_content = msg.content.clone();
        let (user_content, persist_user_content, merged_into_orphan) = {
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
                self.history_message_ids.push(None);
                (new_content.clone(), new_content, false)
            }
        };

        if should_deliver_to_external_channel(self.ctx.session_store.as_ref(), Some(session_id)) {
            if let Ok(id) = session_store.append_message(session_id, "user", &persist_user_content, None) {
                if id > 0 && !merged_into_orphan {
                    if let Some(last) = self.history_message_ids.last_mut() {
                        *last = Some(id);
                    }
                }
            }
        }

        if estimate_tokens(&self.history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS {
            self.run_compact_in_memory(session_id, session_store).await?;
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
                        self.run_compact_in_memory(session_id, session_store)
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
                self.on_message(&text, false, &delivery_channel_name, &delivery_reply_target)?;
                if should_deliver_to_external_channel(
                    self.ctx.session_store.as_ref(),
                    Some(session_id),
                ) {
                    if let Ok(id) = session_store.append_message(session_id, "assistant", &text, None) {
                        self.history_message_ids.push(if id > 0 { Some(id) } else { None });
                    } else {
                        self.history_message_ids.push(None);
                    }
                } else {
                    self.history_message_ids.push(None);
                }
                return Ok(());
            }

            let assistant_content = build_assistant_content_native(&text, &tool_calls);
            self.on_message(&text, false, &delivery_channel_name, &delivery_reply_target)?;
            self.history.push(ChatMessage::assistant(assistant_content));
            self.history_message_ids.push(None);

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
                        self.history_message_ids.push(None);
                        self.on_message(
                            skip_msg,
                            true,
                            &delivery_channel_name,
                            &delivery_reply_target,
                        )?;
                    }
                    let merged = build_steer_merge_message(&user_content, steer);
                    self.history.push(ChatMessage::user(merged));
                    self.history_message_ids.push(None);
                    break;
                }

                let args = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                let tool_args =
                    maybe_bind_source_session_id(&call.name, args, Some(session_id.as_str()));
                let (output, _success) = self.execute_single_tool(&call, tool_args).await;
                let output = maybe_truncate_tool_output(&output);
                self.history.push(ChatMessage::tool(
                    serde_json::json!({
                        "tool_call_id": call.id,
                        "content": output
                    })
                    .to_string(),
                ));
                self.history_message_ids.push(None);
                self.on_message(&output, true, &delivery_channel_name, &delivery_reply_target)?;
            }
        }

        anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
    }

    async fn run_compact_in_memory(
        &mut self,
        session_id: &SessionId,
        session_store: &SessionStore,
    ) -> Result<()> {
        let keep_recent = resolve_keep_recent_messages(self.ctx.session_history_limit);
        let default_resolved = self
            .ctx
            .provider_manager
            .default_resolved()
            .map_err(anyhow::Error::msg)?;
        match compact_in_memory_history(
            &self.history,
            &self.history_message_ids,
            session_store,
            session_id,
            &default_resolved,
            &self.system_prompt,
            keep_recent,
        )
        .await
        {
            Ok((new_history, new_ids, _compacted)) => {
                self.history = new_history;
                self.history_message_ids = new_ids;
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id.as_str(), "In-memory compaction failed: {e}");
            }
        }
        Ok(())
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

    /// Inner loop for memory mode: LLM + tool calls until final text. Returns response or error.
    async fn tool_call_loop_memory_inner(
        &self,
        provider_ctx: &crate::providers::ProviderCtx,
        history: &mut Vec<ChatMessage>,
        source_session_id: Option<&str>,
    ) -> Result<String> {
        let provider_name = "channel-runtime";

        for _iter in 0..MAX_TOOL_ITERATIONS {
            self.ctx.observer.record_event(&ObserverEvent::LlmRequest {
                provider: provider_name.to_string(),
                model: provider_ctx.model.clone(),
                messages_count: history.len(),
            });

            let resp = provider_ctx
                .provider
                .chat(
                    ChatRequest {
                        messages: history.as_slice(),
                        tools: self.tool_slice(),
                    },
                    &provider_ctx.model,
                    provider_ctx.temperature,
                )
                .await?;

            let text = resp.text_or_empty().to_string();
            let tool_calls = resp.tool_calls;

            if tool_calls.is_empty() {
                history.push(ChatMessage::assistant(text.clone()));
                return Ok(text);
            }

            let assistant_content = build_assistant_content_native(&text, &tool_calls);
            let mut execution_results: Vec<ToolExecutionResult> = Vec::with_capacity(tool_calls.len());
            for call in &tool_calls {
                let args = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                let tool_args =
                    maybe_bind_source_session_id(&call.name, args, source_session_id);
                let (output, success) = self.execute_single_tool(call, tool_args).await;
                let output = maybe_truncate_tool_output(&output);
                execution_results.push(ToolExecutionResult {
                    name: call.name.clone(),
                    output,
                    success,
                    tool_call_id: Some(call.id.clone()),
                });
            }
            history.push(ChatMessage::assistant(assistant_content));
            for m in tool_results_to_chat_messages(&execution_results) {
                history.push(m);
            }
        }

        anyhow::bail!("Agent exceeded maximum tool iterations ({MAX_TOOL_ITERATIONS})")
    }

    /// Memory-mode turn: build memory context, run tool call loop, deliver response.
    /// No session history; one turn per message. Uses system_prompt and tool_allow_list from Agent::new.
    async fn tool_call_loop_memory(&mut self, msg: &traits::ChannelMessage) -> Result<()> {
        let active_session = self.session_id.as_ref();
        let (delivery_channel_name, delivery_reply_target) =
            resolve_delivery_target(&self.ctx, active_session, msg);

        let memory_context = build_memory_context(
            self.ctx.memory.as_ref(),
            &msg.content,
            active_session.map(SessionId::as_str),
        )
        .await;
        if self.ctx.auto_save_memory {
            let autosave_key = conversation_memory_key(msg);
            let _ = self.ctx.memory.store(
                &autosave_key,
                &msg.content,
                MemoryCategory::Conversation,
                active_session.map(SessionId::as_str),
            ).await;
        }
        let enriched_message = if memory_context.is_empty() {
            msg.content.clone()
        } else {
            format!("{memory_context}{}", msg.content)
        };

        println!("  ⏳ Processing message...");
        let started_at = Instant::now();

        let mut history = vec![
            ChatMessage::system(AsRef::<str>::as_ref(&self.system_prompt)),
            ChatMessage::user(&enriched_message),
        ];

        let resolved =
            resolve_turn_provider_model_temperature(self.ctx.as_ref(), active_session);

        let llm_result = timeout_future(
            Duration::from_secs(CHANNEL_MESSAGE_TIMEOUT_SECS),
            self.tool_call_loop_memory_inner(
                &resolved,
                &mut history,
                active_session.map(SessionId::as_str),
            ),
        )
        .await;

        let deliver =
            should_deliver_to_external_channel(self.ctx.session_store.as_ref(), active_session);

        match llm_result {
            Ok(Ok(response)) => {
                println!(
                    "  🤖 Reply ({}ms): {}",
                    started_at.elapsed().as_millis(),
                    truncate_with_ellipsis(&response, 80)
                );
                if deliver {
                    let _ = self.on_message(
                        &response,
                        false,
                        &delivery_channel_name,
                        &delivery_reply_target,
                    );
                }
            }
            Ok(Err(e)) => {
                eprintln!(
                    "  ❌ LLM error after {}ms: {e}",
                    started_at.elapsed().as_millis()
                );
                if deliver {
                    let _ = self.on_message(
                        &format!("⚠️ Error: {e}"),
                        false,
                        &delivery_channel_name,
                        &delivery_reply_target,
                    );
                }
            }
            Err(_) => {
                eprintln!(
                    "  ❌ LLM response timed out after {}s (elapsed: {}ms)",
                    CHANNEL_MESSAGE_TIMEOUT_SECS,
                    started_at.elapsed().as_millis()
                );
                if deliver {
                    let _ = self.on_message(
                        "⚠️ Request timed out while waiting for the model. Please try again.",
                        false,
                        &delivery_channel_name,
                        &delivery_reply_target,
                    );
                }
            }
        }

        Ok(())
    }

    /// Memory context loop: one tool_call_loop_memory per message (sequential).
    async fn memory_context_loop(
        mut self,
        _registry: Arc<ParkingMutex<HashMap<String, AgentHandle>>>,
        _key_str: String,
    ) {
        while let Some(work) = self.message_rx.recv().await {
            let msg = work.to_channel_message();
            if let Err(e) = self.tool_call_loop_memory(&msg).await {
                tracing::error!("Memory turn error: {e}");
            }
        }
    }
}

/// Resolve delivery channel and reply target from message; use route metadata when on internal channel and session is present.
fn resolve_delivery_target(
    ctx: &ChannelRuntimeContext,
    active_session: Option<&SessionId>,
    msg: &traits::ChannelMessage,
) -> (String, String) {
    let mut ch = msg.channel.clone();
    let mut rt = msg.reply_target.clone();
    if msg.channel == INTERNAL_MESSAGE_CHANNEL {
        if let (Some(store), Some(session_id)) = (ctx.session_store.as_ref(), active_session) {
            if let Ok(Some(meta)) = store.load_route_metadata(session_id) {
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
