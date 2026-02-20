//! Turn execution: build history, run tool loop, deliver response.
//! Moved from channels so orchestration lives in agent; channels remain transport-only.

use crate::agent::loop_::{build_tool_instructions, run_tool_call_loop};
use crate::channels::{
    build_ephemeral_announce_context, build_memory_context, build_session_turn_history,
    build_system_prompt, channel_delivery_instructions, conversation_memory_key,
    resolve_agent_spec_policy, resolve_effective_provider_model, send_delivery_message,
    should_deliver_to_external_channel, ChannelRuntimeContext, CHANNEL_MESSAGE_TIMEOUT_SECS,
    INTERNAL_MESSAGE_CHANNEL,
};
use crate::channels::traits;
use crate::memory::MemoryCategory;
use crate::providers::{self, ChatMessage};
use crate::session::compaction::{
    build_merged_system_prompt, estimate_tokens, load_compaction_state, maybe_compact,
    CompactionState, SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS,
    SESSION_COMPACTION_KEEP_RECENT_MESSAGES,
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
    let merged_system_prompt = build_merged_system_prompt(
        ctx.system_prompt.as_str(),
        channel_delivery_instructions(&delivery_channel_name),
    );
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
        if let (Some(store), Some(session_id)) = (ctx.session_store.as_ref(), active_session) {
            if let Some(policy) = resolve_agent_spec_policy(store, session_id) {
                let allowed_tools = policy.tools.clone();
                let allowed_skills = policy.skills.clone();
                let tool_entries: Vec<(&str, &str)> = ctx
                    .tools_registry
                    .iter()
                    .filter(|t| {
                        allowed_tools
                            .as_ref()
                            .map(|allow| allow.iter().any(|n| n == t.name()))
                            .unwrap_or(true)
                    })
                    .map(|t| (t.name(), t.description()))
                    .collect();
                let filtered_skills_vec: Vec<crate::skills::Skill> =
                    if let Some(ref allow) = allowed_skills {
                        ctx.all_skills
                            .iter()
                            .filter(|s| allow.iter().any(|n| n == &s.name))
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
                let mut base_prompt = build_system_prompt(
                    &ctx.config.workspace_dir,
                    ctx.model.as_str(),
                    &tool_entries,
                    &filtered_skills_vec,
                    Some(&ctx.config.identity),
                    bootstrap_max_chars,
                );
                base_prompt.push_str(&build_tool_instructions(
                    ctx.tools_registry.as_ref(),
                    allowed_tools.as_deref(),
                ));
                let merged = build_merged_system_prompt(
                    &base_prompt,
                    channel_delivery_instructions(&msg.channel),
                );
                (merged, allowed_tools)
            } else {
                (merged_system_prompt.clone(), None)
            }
        } else {
            (merged_system_prompt.clone(), None)
        };

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
                let keep_recent = usize::try_from(ctx.session_history_limit)
                    .ok()
                    .map(|limit| limit.clamp(1, SESSION_COMPACTION_KEEP_RECENT_MESSAGES))
                    .unwrap_or(SESSION_COMPACTION_KEEP_RECENT_MESSAGES);

                match maybe_compact(
                    &*session_store,
                    session_id,
                    ctx.provider.as_ref(),
                    ctx.model.as_str(),
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

    let (provider_override, model_override, temperature_override) =
        if let (Some(store), Some(session_id)) = (ctx.session_store.as_ref(), active_session) {
            let (eff_provider, eff_model, eff_temp) =
                resolve_effective_provider_model(store, session_id, &ctx.config);
            let default_provider = ctx
                .config
                .default_provider
                .as_deref()
                .unwrap_or("openrouter");
            let default_model = ctx
                .config
                .default_model
                .as_deref()
                .unwrap_or("anthropic/claude-sonnet-4");
            let default_temperature = ctx.config.default_temperature;
            if eff_provider != default_provider
                || eff_model != default_model
                || (eff_temp - default_temperature).abs() > 1e-9
            {
                match providers::create_routed_provider(
                    &eff_provider,
                    ctx.config.api_key.as_deref(),
                    ctx.config.api_url.as_deref(),
                    &ctx.config.reliability,
                    &ctx.config.model_routes,
                    &eff_model,
                ) {
                    Ok(provider) => (Some(Arc::from(provider)), Some(eff_model), Some(eff_temp)),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to create routed provider for session agent: {e}; using default"
                        );
                        (None, None, None)
                    }
                }
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        };

    let provider_ref = provider_override
        .as_deref()
        .unwrap_or_else(|| ctx.provider.as_ref());
    let model_str = model_override
        .as_deref()
        .unwrap_or_else(|| ctx.model.as_str());
    let temp = temperature_override.unwrap_or(ctx.temperature);

    let llm_result = timeout_future(
        Duration::from_secs(CHANNEL_MESSAGE_TIMEOUT_SECS),
        run_tool_call_loop(
            provider_ref,
            &mut history,
            ctx.tools_registry.as_ref(),
            tool_allow_list.as_deref(),
            ctx.observer.as_ref(),
            "channel-runtime",
            model_str,
            temp,
            true,
            None,
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

/// Session turn entrypoint. Used by the per-session agent loop.
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

/// Non-session turn entrypoint. Used by direct dispatch path when session mode is disabled.
pub(crate) async fn run_memory_turn(
    ctx: Arc<ChannelRuntimeContext>,
    active_session: Option<&SessionId>,
    msg: traits::ChannelMessage,
) -> Result<()> {
    run_turn_core(ctx, active_session, msg, None, false).await
}
