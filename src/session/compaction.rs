use crate::providers::ChatRequest;
use crate::providers::ChatMessage;
use crate::session::{SessionId, SessionStore};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::fmt::Write;

pub const SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY: &str = "compaction_after_message_id";
pub const SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS: usize = 12_000;
pub const SESSION_COMPACTION_KEEP_RECENT_MESSAGES: usize = 24;
const SESSION_COMPACTION_MAX_SOURCE_CHARS: usize = 32_000;
const SESSION_COMPACTION_MAX_SUMMARY_CHARS: usize = 6_000;

#[derive(Debug, Clone, Default)]
pub struct CompactionState {
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

/// Build the in-memory and persisted compaction summary as an assistant message.
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
    let after_message_id = store
        .get_state_key(session_id, SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY)?
        .and_then(|raw| decode_json_i64(&raw));

    Ok(CompactionState {
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

fn build_compaction_prompt(transcript: &str, system_prompt: &str) -> String {
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
    let _ = writeln!(prompt, "Transcript to compact:");
    let _ = writeln!(prompt, "{transcript}");
    prompt
}

fn is_tool_call_message(msg: &ChatMessage) -> bool {
    msg.role == "tool" || (msg.role == "assistant" && msg.content.contains("tool_call_id"))
}

/// Compact in-memory history: summarize messages *before* the last user, keep last user and rest.
/// Summary is persisted as an assistant message and placed in memory before that last user.
/// last_id = current last message id in DB (end boundary). Returns (new_history, compacted).
pub async fn compact_in_memory_history(
    history: &[ChatMessage],
    store: &SessionStore,
    session_id: &SessionId,
    provider_ctx: &crate::providers::ProviderCtx,
    system_prompt: &str,
    _keep_recent: usize,
) -> Result<(Vec<ChatMessage>, bool)> {
    let last_user_idx = history.iter().rposition(|m| m.role == "user");
    let Some(last_user_idx) = last_user_idx else {
        return Ok((history.to_vec(), false));
    };

    let to_compact = history.get(1..last_user_idx).unwrap_or_default();
    let to_compact_for_transcript: Vec<ChatMessage> = to_compact
        .iter()
        .filter(|m| !is_tool_call_message(m))
        .cloned()
        .collect();

    if to_compact_for_transcript.is_empty() {
        return Ok((history.to_vec(), false));
    }

    let last_id = store.last_message_id(session_id).ok().flatten().filter(|&id| id > 0);

    let transcript = build_transcript_from_chat_messages(&to_compact_for_transcript);
    let prompt = build_compaction_prompt(&transcript, system_prompt);

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

    if let Some(boundary_id) = last_id {
        let _ = store.set_state_key(
            session_id,
            SESSION_COMPACTION_AFTER_MESSAGE_ID_KEY,
            &serde_json::to_string(&boundary_id).unwrap_or_default(),
        );
    }

    let summary_msg = build_compaction_summary_message(&summary);
    let summary_content = summary_msg.content.clone();
    let _ = store.append_message(session_id, "assistant", &summary_content, None);

    let kept = history.get(last_user_idx..).unwrap_or_default();
    let mut new_history = vec![history[0].clone()];
    new_history.push(summary_msg);
    new_history.extend(kept.to_vec());

    Ok((new_history, true))
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
