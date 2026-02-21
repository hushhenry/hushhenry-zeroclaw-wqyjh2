//! Turn execution: build history, run tool loop, deliver response.
//! Moved from channels so orchestration lives in agent; channels remain transport-only.

use crate::agent::loop_::run_tool_call_loop;
use crate::channels::traits;
use crate::channels::{
    build_ephemeral_announce_context, build_memory_context, build_session_turn_history,
    conversation_memory_key, resolve_effective_system_prompt_and_tool_allow_list,
    resolve_turn_provider_model_temperature, send_delivery_message,
    should_deliver_to_external_channel, ChannelRuntimeContext, CHANNEL_MESSAGE_TIMEOUT_SECS,
    INTERNAL_MESSAGE_CHANNEL,
};
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::session::compaction::{
    estimate_tokens, load_compaction_state, maybe_compact, resolve_keep_recent_messages,
    CompactionOutcome, CompactionState, SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS,
};
use crate::session::SessionId;
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::convert::AsRef;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout as timeout_future;

/// Core turn execution: build history, run tool loop, deliver response.
/// `use_session_history` controls whether session history compaction/persistence logic is enabled.
#[deprecated(
    since = "0.1.0",
    note = "Replaced by Agent in agent/agent.rs; use Agent::tool_call_loop_session for session mode."
)]
pub(crate) async fn run_turn_core(
    ctx: Arc<ChannelRuntimeContext>,
    active_session: Option<&SessionId>,
    msg: traits::ChannelMessage,
    #[allow(clippy::type_complexity)] steer_at_checkpoint: Option<
        &mut (dyn FnMut(&str) -> Option<String> + Send + '_),
    >,
    use_session_history: bool,
) -> Result<()> {
    let mut delivery_channel_name = msg.channel.clone();
    let mut delivery_reply_target = msg.reply_target.clone();
    if msg.channel == INTERNAL_MESSAGE_CHANNEL {
        if let (Some(store), Some(session_id)) = (ctx.session_store.as_ref(), active_session) {
            if let Ok(Some(meta)) = store.load_route_metadata(session_id) {
                if meta.channel != INTERNAL_MESSAGE_CHANNEL {
                    delivery_reply_target = meta.route_id.unwrap_or(meta.chat_id);
                    delivery_channel_name = meta.channel;
                }
            }
        }
    }

    let target_channel = ctx.channels_by_name.get(&delivery_channel_name).cloned();
    let enriched_message = if use_session_history {
        msg.content.clone()
    } else {
        let memory_context = build_memory_context(
            ctx.memory.as_ref(),
            &msg.content,
            active_session.map(SessionId::as_str),
        )
        .await;
        if ctx.auto_save_memory {
            let autosave_key = conversation_memory_key(&msg);
            let _ = ctx
                .memory
                .store(
                    &autosave_key,
                    &msg.content,
                    MemoryCategory::Conversation,
                    active_session.map(SessionId::as_str),
                )
                .await;
        }
        if memory_context.is_empty() {
            msg.content.clone()
        } else {
            format!("{memory_context}{}", msg.content)
        }
    };

    if let Some(channel) = target_channel.as_ref() {
        if let Err(e) = channel.start_typing(&delivery_reply_target).await {
            tracing::debug!("Failed to start typing on {}: {e}", channel.name());
        }
    }

    println!("  ⏳ Processing message...");
    let started_at = Instant::now();

    let (effective_system_prompt, tool_allow_list) =
        resolve_effective_system_prompt_and_tool_allow_list(
            ctx.as_ref(),
            active_session,
            &delivery_channel_name,
        );

    let mut history: Vec<ChatMessage>;

    if use_session_history {
        if let (Some(session_store), Some(session_id)) =
            (ctx.session_store.as_ref(), active_session)
        {
            let mut compaction_state = match load_compaction_state(session_store, session_id) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(
                        "Failed to load compaction state for session {}: {error}",
                        session_id.as_str()
                    );
                    CompactionState::default()
                }
            };

            let mut tail_messages = match session_store
                .load_messages_after_id(session_id, compaction_state.after_message_id)
            {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!(
                        "Failed to load session tail history {}: {error}",
                        session_id.as_str()
                    );
                    Vec::new()
                }
            };

            let ephemeral = build_ephemeral_announce_context(&tail_messages);
            let user_content = if ephemeral.is_empty() {
                enriched_message.clone()
            } else {
                format!("{ephemeral}\n\n{enriched_message}")
            };
            history = build_session_turn_history(
                &effective_system_prompt,
                &compaction_state,
                &tail_messages,
                &user_content,
            );

            if estimate_tokens(&history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS {
                let keep_recent = resolve_keep_recent_messages(ctx.session_history_limit);
                let default_resolved = ctx
                    .provider_manager
                    .default_resolved()
                    .unwrap_or_else(|e| panic!("default_resolved failed: {e}"));

                match maybe_compact(
                    &*session_store,
                    session_id,
                    &default_resolved,
                    &effective_system_prompt,
                    keep_recent,
                )
                .await
                {
                    Ok(outcome) if outcome.compacted => {
                        compaction_state.summary = outcome.summary;
                        compaction_state.after_message_id = outcome.after_message_id;
                        tail_messages = session_store
                            .load_messages_after_id(session_id, compaction_state.after_message_id)
                            .unwrap_or_default();
                        history = build_session_turn_history(
                            &effective_system_prompt,
                            &compaction_state,
                            &tail_messages,
                            &enriched_message,
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            "Auto-compaction failed for session {}: {error}",
                            session_id.as_str()
                        );
                    }
                }
            }
        } else {
            history = vec![
                ChatMessage::system(AsRef::<str>::as_ref(&effective_system_prompt)),
                ChatMessage::user(&enriched_message),
            ];
        }
    } else {
        history = vec![
            ChatMessage::system(AsRef::<str>::as_ref(&effective_system_prompt)),
            ChatMessage::user(&enriched_message),
        ];
    }

    let resolved = resolve_turn_provider_model_temperature(ctx.as_ref(), active_session);

    let llm_result = timeout_future(
        Duration::from_secs(CHANNEL_MESSAGE_TIMEOUT_SECS),
        run_tool_call_loop(
            &resolved,
            &mut history,
            ctx.tools_registry.as_ref(),
            tool_allow_list.as_deref(),
            ctx.observer.as_ref(),
            "channel-runtime",
            true,
            delivery_channel_name.as_str(),
            active_session.map(SessionId::as_str),
            steer_at_checkpoint,
        ),
    )
    .await;

    let deliver = should_deliver_to_external_channel(ctx.session_store.as_ref(), active_session);
    if deliver {
        if let Some(channel) = target_channel.as_ref() {
            if let Err(e) = channel.stop_typing(&delivery_reply_target).await {
                tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
            }
        }
    }

    match llm_result {
        Ok(Ok(response)) => {
            println!(
                "  🤖 Reply ({}ms): {}",
                started_at.elapsed().as_millis(),
                truncate_with_ellipsis(&response, 80)
            );
            if deliver {
                if let Err(e) = send_delivery_message(
                    target_channel.as_ref(),
                    &delivery_channel_name,
                    &delivery_reply_target,
                    &response,
                )
                .await
                {
                    eprintln!("  ❌ Failed to reply on {}: {e}", delivery_channel_name);
                }
            }

            if use_session_history {
                if let (Some(session_store), Some(session_id)) =
                    (ctx.session_store.as_ref(), active_session)
                {
                    // Persist the user message we actually replied to (may differ from msg.content
                    // when steer-merge injected merged content).
                    let user_content = history
                        .iter()
                        .rev()
                        .find(|m| m.role == "user" && !m.content.starts_with("[Tool results]"))
                        .map(|m| m.content.as_str())
                        .unwrap_or(msg.content.as_str());
                    if let Err(error) =
                        session_store.append_message(session_id, "user", user_content, None)
                    {
                        tracing::warn!(
                            "Failed to persist user session message {}: {error}",
                            session_id.as_str()
                        );
                    }
                    if let Err(error) =
                        session_store.append_message(session_id, "assistant", &response, None)
                    {
                        tracing::warn!(
                            "Failed to persist assistant session message {}: {error}",
                            session_id.as_str()
                        );
                    }
                }
            }
        }
        Ok(Err(e)) => {
            eprintln!(
                "  ❌ LLM error after {}ms: {e}",
                started_at.elapsed().as_millis()
            );
            if deliver {
                if let Err(send_err) = send_delivery_message(
                    target_channel.as_ref(),
                    &delivery_channel_name,
                    &delivery_reply_target,
                    &format!("⚠️ Error: {e}"),
                )
                .await
                {
                    tracing::debug!(
                        "Failed to send model error message on {}: {send_err}",
                        delivery_channel_name
                    );
                }
            }
        }
        Err(_) => {
            let timeout_msg = format!(
                "LLM response timed out after {}s",
                CHANNEL_MESSAGE_TIMEOUT_SECS
            );
            eprintln!(
                "  ❌ {} (elapsed: {}ms)",
                timeout_msg,
                started_at.elapsed().as_millis()
            );
            if deliver {
                if let Err(send_err) = send_delivery_message(
                    target_channel.as_ref(),
                    &delivery_channel_name,
                    &delivery_reply_target,
                    "⚠️ Request timed out while waiting for the model. Please try again.",
                )
                .await
                {
                    tracing::debug!(
                        "Failed to send timeout message on {}: {send_err}",
                        delivery_channel_name
                    );
                }
            }
        }
    }

    Ok(())
}

/// Session turn entrypoint. Replaced by Agent::session_context_loop + tool_call_loop_session.
#[deprecated(
    since = "0.1.0",
    note = "Session mode now uses Agent in agent/agent.rs."
)]
pub(crate) async fn run_session_turn(
    ctx: Arc<ChannelRuntimeContext>,
    session_id: &SessionId,
    msg: traits::ChannelMessage,
    #[allow(clippy::type_complexity)] steer_at_checkpoint: Option<
        &mut (dyn FnMut(&str) -> Option<String> + Send + '_),
    >,
) -> Result<()> {
    run_turn_core(ctx, Some(session_id), msg, steer_at_checkpoint, true).await
}

/// Non-session turn entrypoint. Used by Agent::memory_context_loop (spawned per message).
pub(crate) async fn run_memory_turn(
    ctx: Arc<ChannelRuntimeContext>,
    active_session: Option<&SessionId>,
    msg: traits::ChannelMessage,
) -> Result<()> {
    run_turn_core(ctx, active_session, msg, None, false).await
}

/// Run session compaction once (effective system prompt + maybe_compact).
/// Used by manual /compact and shares the same prompt resolution as the agent turn.
pub(crate) async fn run_session_compaction(
    ctx: &ChannelRuntimeContext,
    session_id: &SessionId,
    channel_name: &str,
) -> Result<CompactionOutcome> {
    let session_store = ctx
        .session_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Session store is unavailable"))?;
    let (effective_system_prompt, _) =
        resolve_effective_system_prompt_and_tool_allow_list(ctx, Some(session_id), channel_name);
    let keep_recent = resolve_keep_recent_messages(ctx.session_history_limit);
    let default_resolved = ctx
        .provider_manager
        .default_resolved()
        .unwrap_or_else(|e| panic!("default_resolved failed: {e}"));
    maybe_compact(
        session_store.as_ref(),
        session_id,
        &default_resolved,
        &effective_system_prompt,
        keep_recent,
    )
    .await
}
