//! Turn execution: session compaction and related helpers.
//! Memory-mode turn lives on Agent as Agent::tool_call_loop_memory.

use crate::channels::{
    build_session_turn_history_with_tail, normalize_tail_messages,
    resolve_effective_system_prompt_and_tool_allow_list, ChannelRuntimeContext,
};
use crate::session::compaction::{
    compact_in_memory_history, load_compaction_state, resolve_keep_recent_messages,
    CompactionOutcome,
};
use crate::session::SessionId;
use anyhow::Result;

/// Run session compaction once: load tail from DB, run in-memory compaction, write summary and boundary.
/// Used by manual /compact.
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
        .map_err(anyhow::Error::msg)?;

    let compaction_state =
        load_compaction_state(session_store.as_ref(), session_id).unwrap_or_default();
    let tail_messages = session_store
        .load_messages_after_id(session_id, compaction_state.after_message_id)
        .unwrap_or_default();
    let (tail_chat, _) = normalize_tail_messages(&tail_messages);
    let history =
        build_session_turn_history_with_tail(&effective_system_prompt, &tail_chat, None);

    let (_, compacted) = compact_in_memory_history(
        &history,
        session_store.as_ref(),
        session_id,
        &default_resolved,
        &effective_system_prompt,
        keep_recent,
    )
    .await?;

    Ok(CompactionOutcome {
        compacted,
        summary: None,
        after_message_id: None,
    })
}
