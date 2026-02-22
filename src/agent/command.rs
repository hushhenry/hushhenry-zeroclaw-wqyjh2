//! Slash-command parsing and handling. Lives in agent so command semantics
//! are owned by the agent layer; channels only dispatch and send replies.

use crate::channels::traits::ChannelMessage;
use crate::channels::{
    dispatch_outbound_message, outbound_key_from_parts, SendMessage,
    ChannelRuntimeContext,
};
use crate::session::{SessionId, SessionStore};
use std::fmt::Write;

const COMMAND_LIST_LIMIT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Stop,
    New,
    Compact,
    Agents,
    AgentSwitch { id_or_name: String },
    Models,
    Model { provider_model: String },
}

/// Parses a slash command from message content. Returns `None` if the content
/// does not start with a supported command.
pub fn parse_slash_command(content: &str) -> Option<SlashCommand> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command = parts.next()?;

    match command {
        "/stop" => Some(SlashCommand::Stop),
        "/new" => Some(SlashCommand::New),
        "/compact" => Some(SlashCommand::Compact),
        "/agents" => Some(SlashCommand::Agents),
        "/agent" => Some(SlashCommand::AgentSwitch {
            id_or_name: parts.next().map(str::trim).unwrap_or_default().to_string(),
        }),
        "/models" => Some(SlashCommand::Models),
        "/model" => Some(SlashCommand::Model {
            provider_model: parts.next().map(str::trim).unwrap_or_default().to_string(),
        }),
        _ => None,
    }
}

/// True for commands that are executed in the agent loop (enqueued to agent, not handled in gateway).
pub(crate) fn is_agent_command(cmd: &SlashCommand) -> bool {
    matches!(
        cmd,
        SlashCommand::Stop
            | SlashCommand::New
            | SlashCommand::Compact
            | SlashCommand::Models
            | SlashCommand::Model { .. }
    )
}

/// Send a command reply via dispatch_outbound_message (single outbound path).
async fn dispatch_reply(msg: &ChannelMessage, content: String) {
    let key = outbound_key_from_parts(&msg.channel, &msg.reply_target);
    let sm = SendMessage::new(content, msg.reply_target.clone());
    if let Err(e) = dispatch_outbound_message(key, sm).await {
        tracing::error!("Failed to send command response: {e}");
    }
}

/// Short display form of a session id (first 8 chars).
pub(crate) fn short_session_id(session_id: &SessionId) -> String {
    session_id.as_str().chars().take(8).collect::<String>()
}

/// Handles a parsed slash command in the context of a channel message. Only processes
/// global-level commands (e.g. /agents); agent-level commands must be enqueued to the
/// agent loop by the caller. Returns `true` if the message was consumed.
pub(crate) async fn handle_slash_command(
    ctx: &ChannelRuntimeContext,
    msg: &ChannelMessage,
    inbound_key: &str,
    command: SlashCommand,
) -> bool {
    let Some(session_store) = ctx.session_store.as_ref() else {
        dispatch_reply(&msg, "Session store is unavailable.".to_string()).await;
        return true;
    };

    match command {
        SlashCommand::Stop
        | SlashCommand::New
        | SlashCommand::Compact
        | SlashCommand::Models
        | SlashCommand::Model { .. } => {
            unreachable!(
                "handle_slash_command only handles global commands; agent commands must be enqueued to agent"
            );
        }
        SlashCommand::Agents => match session_store.get_or_create_active(inbound_key) {
            Ok(session_id) => {
                let agents = session_store
                    .list_agents(COMMAND_LIST_LIMIT)
                    .unwrap_or_default();
                let active_id = session_store
                    .get_state_key(&session_id, SessionStore::ACTIVE_AGENT_ID_KEY)
                    .ok()
                    .flatten()
                    .and_then(|raw| {
                        serde_json::from_str::<String>(&raw)
                            .ok()
                            .or_else(|| (!raw.is_empty()).then(|| raw))
                    });
                let mut text = String::new();
                let _ = writeln!(text, "Agents (session `{}`)", short_session_id(&session_id));
                let _ = writeln!(
                    text,
                    "Active: {}",
                    active_id.as_deref().unwrap_or("(default)")
                );
                for agent in agents.iter().take(10) {
                    let marker = if active_id.as_deref() == Some(agent.agent_id.as_str()) {
                        "* "
                    } else {
                        "- "
                    };
                    let _ = writeln!(text, "{marker}{} — {}", agent.name, agent.agent_id);
                }
                let _ = writeln!(text, "Use /agent <id|name> to switch.");
                dispatch_reply(&msg, text.trim().to_string()).await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to resolve session for /agents ({}): {error}",
                    inbound_key
                );
                dispatch_reply(&msg, "⚠️ Failed to list agents.".to_string()).await;
            }
        },
        SlashCommand::AgentSwitch { id_or_name } => {
            if id_or_name.is_empty() {
                dispatch_reply(
                    &msg,
                    "Usage: /agent <agent_id|agent_name>. Use /agents to list.".to_string(),
                )
                .await;
                return true;
            }
            match session_store.get_or_create_active(inbound_key) {
                Ok(session_id) => {
                    let agent = session_store
                        .get_agent_by_id(id_or_name.trim())
                        .ok()
                        .flatten()
                        .or_else(|| {
                            session_store
                                .get_agent_by_name(id_or_name.trim())
                                .ok()
                                .flatten()
                        });
                    match agent {
                        Some(agent) => {
                            let value_json = serde_json::to_string(&agent.agent_id)
                                .unwrap_or_else(|_| format!("\"{}\"", agent.agent_id));
                            if let Err(e) = session_store.set_state_key(
                                &session_id,
                                SessionStore::ACTIVE_AGENT_ID_KEY,
                                &value_json,
                            ) {
                                tracing::error!("Failed to set active_agent_id: {e}");
                                dispatch_reply(&msg, "⚠️ Failed to switch agent.".to_string())
                                    .await;
                                return true;
                            }
                            dispatch_reply(
                                &msg,
                                format!(
                                    "Switched to agent `{}` ({}) for this session.",
                                    agent.name,
                                    &agent.agent_id[..agent.agent_id.len().min(8)]
                                ),
                            )
                            .await;
                        }
                        None => {
                            dispatch_reply(
                                &msg,
                                format!(
                                    "No agent found for `{id_or_name}`. Use /agents to list (match by id or name)."
                                ),
                            )
                            .await;
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(
                        "Failed to resolve session for /agent ({}): {error}",
                        inbound_key
                    );
                    dispatch_reply(&msg, "⚠️ Failed to switch agent.".to_string()).await;
                }
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slash_command_recognizes_supported_commands() {
        assert_eq!(parse_slash_command("/stop"), Some(SlashCommand::Stop));
        assert_eq!(parse_slash_command("/new"), Some(SlashCommand::New));
        assert_eq!(
            parse_slash_command("   /compact"),
            Some(SlashCommand::Compact)
        );
        assert_eq!(parse_slash_command("/agents"), Some(SlashCommand::Agents));
        assert_eq!(
            parse_slash_command("/agent coder"),
            Some(SlashCommand::AgentSwitch {
                id_or_name: "coder".to_string()
            })
        );
        assert_eq!(parse_slash_command("/models"), Some(SlashCommand::Models));
        assert_eq!(
            parse_slash_command("/model openrouter/anthropic/claude-sonnet-4"),
            Some(SlashCommand::Model {
                provider_model: "openrouter/anthropic/claude-sonnet-4".to_string()
            })
        );
    }

    #[test]
    fn is_agent_command_returns_true_for_agent_commands() {
        assert!(is_agent_command(&SlashCommand::Stop));
        assert!(is_agent_command(&SlashCommand::New));
        assert!(is_agent_command(&SlashCommand::Compact));
        assert!(is_agent_command(&SlashCommand::Models));
        assert!(is_agent_command(&SlashCommand::Model {
            provider_model: "p/m".to_string()
        }));
    }

    #[test]
    fn parse_slash_command_rejects_unsupported_or_partial_commands() {
        assert_eq!(parse_slash_command("hello"), None);
        assert_eq!(parse_slash_command("/new-session"), None);
        assert_eq!(parse_slash_command("/sessionss"), None);
        assert_eq!(parse_slash_command("/unknown"), None);
    }
}
