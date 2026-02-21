use crate::providers::ChatRequest;
use crate::providers::{ChatMessage, Provider};
use crate::session::{SessionId, SessionStore};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::fmt::Write;

pub const SESSION_COMPACTION_SUMMARY_KEY: &str = "compaction_summary";
pub const SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY: &str = "compaction_after_message_id";
pub const SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS: usize = 12_000;
pub const SESSION_COMPACTION_KEEP_RECENT_MESSAGES: usize = 24;
const SESSION_COMPACTION_MAX_SOURCE_CHARS: usize = 32_000;
const SESSION_COMPACTION_MAX_SUMMARY_CHARS: usize = 6_000;

#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    pub summary: Option<String>,
    pub after_message_id: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct CompactionOutcome {
    pub compacted: bool,
    pub summary: Option<String>,
    pub after_message_id: Option<i64>,
}

fn decode_json_string(value_json: &str) -> Option<String> {
    serde_json::from_str::<String>(value_json).ok().or_else(|| {
        let trimmed = value_json.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn decode_json_i64(value_json: &str) -> Option<i64> {
    serde_json::from_str::<i64>(value_json).ok().or_else(|| {
        value_json
            .trim_matches('"')
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
    })
}

pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    let char_count: usize = messages
        .iter()
        .map(|msg| msg.content.chars().count() + 16)
        .sum();
    char_count.div_ceil(4)
}

pub fn resolve_keep_recent_messages(session_history_limit: u32) -> usize {
    usize::try_from(session_history_limit)
        .ok()
        .map(|limit| limit.clamp(1, SESSION_COMPACTION_KEEP_RECENT_MESSAGES))
        .unwrap_or(SESSION_COMPACTION_KEEP_RECENT_MESSAGES)
}

pub fn build_compaction_summary_message(summary: &str) -> ChatMessage {
    ChatMessage::assistant(format!("[Session Compaction Summary]\n{}", summary.trim()))
}

pub fn build_merged_system_prompt(
    base_system_prompt: &str,
    channel_instructions: Option<&str>,
) -> String {
    match channel_instructions {
        Some(instructions) if !instructions.trim().is_empty() => format!(
            "{base_system_prompt}\n\n[Channel Delivery Instructions]\n{}",
            instructions.trim()
        ),
        _ => base_system_prompt.to_string(),
    }
}

pub fn load_compaction_state(
    store: &SessionStore,
    session_id: &SessionId,
) -> Result<CompactionState> {
    let summary = store
        .get_state_key(session_id, SESSION_COMPACTION_SUMMARY_KEY)?
        .and_then(|raw| decode_json_string(&raw))
        .map(|summary| {
            truncate_with_ellipsis(summary.trim(), SESSION_COMPACTION_MAX_SUMMARY_CHARS)
        });

    let after_message_id = store
        .get_state_key(session_id, SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY)?
        .and_then(|raw| decode_json_i64(&raw));

    Ok(CompactionState {
        summary,
        after_message_id,
    })
}

/// Build transcript from in-memory ChatMessages for summarization.
pub fn build_transcript_from_chat_messages(messages: &[ChatMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = msg.role.to_uppercase();
        let _ = writeln!(transcript, "{role}: {}", msg.content.trim());
    }

    if transcript.chars().count() > SESSION_COMPACTION_MAX_SOURCE_CHARS {
        truncate_with_ellipsis(&transcript, SESSION_COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

fn build_compaction_prompt(
    existing_summary: Option<&str>,
    transcript: &str,
    system_prompt: &str,
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(prompt, "Goal:");
    let _ = writeln!(
        prompt,
        "- Produce a compact durable session summary for future turns."
    );
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Instructions:");
    let _ = writeln!(
        prompt,
        "- Keep only durable facts: decisions, requirements, unresolved tasks, constraints, preferences."
    );
    let _ = writeln!(prompt, "- Drop filler, repetition, and verbose logs.");
    let _ = writeln!(
        prompt,
        "- Preserve tool outcomes and file paths only when still relevant."
    );
    let _ = writeln!(prompt, "- Be concise and deterministic.");
    let _ = writeln!(prompt);
    let _ = writeln!(
        prompt,
        "Output format (use all sections, omit empty bullet items):"
    );
    let _ = writeln!(prompt, "Discoveries:");
    let _ = writeln!(prompt, "- ...");
    let _ = writeln!(prompt, "Accomplished:");
    let _ = writeln!(prompt, "- ...");
    let _ = writeln!(prompt, "Relevant files:");
    let _ = writeln!(prompt, "- ...");
    let _ = writeln!(prompt);
    let _ = writeln!(prompt, "Stable system prompt:");
    let _ = writeln!(prompt, "{system_prompt}");
    let _ = writeln!(prompt);
    if let Some(summary) = existing_summary {
        let _ = writeln!(prompt, "Previous summary:");
        let _ = writeln!(prompt, "{summary}");
        let _ = writeln!(prompt);
    }
    let _ = writeln!(prompt, "Transcript to compact:");
    let _ = writeln!(prompt, "{transcript}");
    prompt
}

/// Index of the first tail message in history (after system and optional compaction summary).
fn tail_start_index(history: &[ChatMessage]) -> usize {
    if history.is_empty() {
        return 0;
    }
    let mut start = 1;
    if history.len() > 1
        && history[1]
            .content
            .starts_with("[Session Compaction Summary]")
    {
        start = 2;
    }
    start
}

/// Compact in-memory history: summarize old tail, keep recent, write summary and boundary to store.
/// Returns (new_history, new_history_message_ids, compacted).
pub async fn compact_in_memory_history(
    history: &[ChatMessage],
    history_message_ids: &[Option<i64>],
    store: &SessionStore,
    session_id: &SessionId,
    provider_ctx: &crate::providers::ProviderCtx,
    system_prompt: &str,
    keep_recent: usize,
) -> Result<(Vec<ChatMessage>, Vec<Option<i64>>, bool)> {
    let keep_recent = keep_recent.max(1);
    let tail_start = tail_start_index(history);
    let tail = &history[tail_start..];
    let mut tail_ids: Vec<Option<i64>> = history_message_ids
        .get(tail_start..)
        .map(|s| s.iter().take(tail.len()).copied().collect())
        .unwrap_or_default();
    while tail_ids.len() < tail.len() {
        tail_ids.push(None);
    }

    if tail.len() <= keep_recent {
        return Ok((history.to_vec(), history_message_ids.to_vec(), false));
    }

    let compact_until = tail.len().saturating_sub(keep_recent);
    let to_compact = &tail[..compact_until];
    let kept_tail = &tail[compact_until..];
    let kept_ids: Vec<Option<i64>> = tail_ids[compact_until..].to_vec();

    let existing_summary = load_compaction_state(store, session_id)
        .ok()
        .and_then(|s| s.summary);
    let transcript = build_transcript_from_chat_messages(to_compact);
    let prompt = build_compaction_prompt(
        existing_summary.as_deref(),
        &transcript,
        system_prompt,
    );

    let summary_messages = vec![
        ChatMessage::system("You are a session compaction engine. Return compact durable context."),
        ChatMessage::user(prompt),
    ];
    let summary_raw = provider_ctx
        .provider
        .chat(
            ChatRequest {
                messages: &summary_messages,
                tools: None,
            },
            &provider_ctx.model,
            0.1,
        )
        .await
        .map(|response| response.text_or_empty().to_string())
        .unwrap_or_else(|_| {
            truncate_with_ellipsis(&transcript, SESSION_COMPACTION_MAX_SUMMARY_CHARS)
        });
    let summary = truncate_with_ellipsis(summary_raw.trim(), SESSION_COMPACTION_MAX_SUMMARY_CHARS);

    let after_message_id = (0..compact_until)
        .rev()
        .find_map(|i| tail_ids.get(i).and_then(|o| *o))
        .filter(|&id| id > 0);

    if let Some(boundary_id) = after_message_id {
        let _ = store.set_state_key(
            session_id,
            SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY,
            &serde_json::to_string(&boundary_id).unwrap_or_default(),
        );
    }
    let _ = store.set_state_key(
        session_id,
        SESSION_COMPACTION_SUMMARY_KEY,
        &serde_json::to_string(&summary).unwrap_or_default(),
    );

    let new_summary_msg = build_compaction_summary_message(&summary);
    let mut new_history = vec![history[0].clone()];
    new_history.push(new_summary_msg);
    new_history.extend(kept_tail.to_vec());

    let mut new_ids = vec![None; 2];
    new_ids.extend(kept_ids);

    Ok((new_history, new_ids, true))
}

#[cfg(test)]
mod tests {
    use super::{decode_json_i64, decode_json_string, estimate_tokens};
    use crate::providers::ChatMessage;

    #[test]
    fn decode_state_values_from_json_or_plain() {
        assert_eq!(
            decode_json_string("\"summary\""),
            Some("summary".to_string())
        );
        assert_eq!(decode_json_string("summary"), Some("summary".to_string()));
        assert_eq!(decode_json_i64("42"), Some(42));
        assert_eq!(decode_json_i64("\"42\""), Some(42));
    }

    #[test]
    fn token_estimate_is_conservative() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::assistant("alpha beta gamma"),
        ];
        assert!(estimate_tokens(&messages) >= 5);
    }
}
