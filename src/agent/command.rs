//! Slash-command parsing and handling. Lives in agent so command semantics
//! are owned by the agent layer; channels only dispatch and send replies.

use crate::channels::traits::{ChannelMessage, SendMessage};
use crate::channels::{Channel, ChannelRuntimeContext};
use crate::session::{SessionId, SessionStore};
use std::fmt::Write;
use std::sync::Arc;

pub(crate) const SESSION_QUEUE_MODE_KEY: &str = "queue_mode";
const DEFAULT_QUEUE_MODE: &str = "steer-merge";
const COMMAND_LIST_LIMIT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Stop,
    New,
    Compact,
    Queue { mode: Option<String> },
    Subagents,
    Sessions,
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
        "/queue" => Some(SlashCommand::Queue {
            mode: parts.next().map(str::to_string),
        }),
        "/subagents" => Some(SlashCommand::Subagents),
        "/sessions" => Some(SlashCommand::Sessions),
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

/// Send a command reply to the channel. Used by the gateway when agent commands cannot be enqueued (e.g. no session store).
pub(crate) async fn send_command_response(
    target_channel: Option<&Arc<dyn Channel>>,
    reply_target: &str,
    content: String,
) {
    if let Some(channel) = target_channel {
        if let Err(error) = channel.send(&SendMessage::new(content, reply_target)).await {
            tracing::error!(
                "Failed to send command response on {}: {error}",
                channel.name()
            );
        }
    }
}

/// Short display form of a session id (first 8 chars).
pub(crate) fn short_session_id(session_id: &SessionId) -> String {
    session_id.as_str().chars().take(8).collect::<String>()
}

fn current_queue_mode(session_store: &SessionStore, session_id: &SessionId) -> String {
    crate::channels::decode_session_string_state(
        session_store
            .get_state_key(session_id, SESSION_QUEUE_MODE_KEY)
            .ok()
            .flatten(),
    )
    .unwrap_or_else(|| DEFAULT_QUEUE_MODE.to_string())
}

/// Handles a parsed slash command in the context of a channel message. Returns `true`
/// if the message was consumed by the command (caller should not process as normal message).
pub(crate) async fn handle_slash_command(
    ctx: &ChannelRuntimeContext,
    target_channel: Option<&Arc<dyn Channel>>,
    msg: &ChannelMessage,
    inbound_key: &str,
    command: SlashCommand,
) -> bool {
    let Some(session_store) = ctx.session_store.as_ref() else {
        send_command_response(
            target_channel,
            &msg.reply_target,
            "Session store is unavailable.".to_string(),
        )
        .await;
        return true;
    };

    match command {
        SlashCommand::Stop => {
            send_command_response(
                target_channel,
                &msg.reply_target,
                "Agent was aborted.".to_string(),
            )
            .await;
            return true;
        }
        SlashCommand::New => match session_store.create_new(inbound_key) {
            Ok(session_id) => {
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    format!(
                        "Started a new session `{}` for this conversation.",
                        short_session_id(&session_id)
                    ),
                )
                .await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to create new session for key {}: {error}",
                    inbound_key
                );
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "⚠️ Failed to create a new session. Please try again.".to_string(),
                )
                .await;
            }
        },
        SlashCommand::Compact => unreachable!("Compact is an agent command; handled via agent.cmd_compact -> run_compact_in_memory"),
        SlashCommand::Queue { mode } => match session_store.get_or_create_active(inbound_key) {
            Ok(session_id) => {
                if let Some(mode) = mode {
                    if mode != DEFAULT_QUEUE_MODE {
                        send_command_response(
                            target_channel,
                            &msg.reply_target,
                            format!(
                                "Unsupported queue mode `{mode}`. Supported modes: `{DEFAULT_QUEUE_MODE}`."
                            ),
                        )
                        .await;
                        return true;
                    }

                    if let Err(error) = session_store.set_state_key(
                        &session_id,
                        SESSION_QUEUE_MODE_KEY,
                        &serde_json::to_string(DEFAULT_QUEUE_MODE)
                            .unwrap_or_else(|_| format!("\"{DEFAULT_QUEUE_MODE}\"")),
                    ) {
                        tracing::error!(
                            "Failed to persist queue mode for session {}: {error}",
                            session_id.as_str()
                        );
                        send_command_response(
                            target_channel,
                            &msg.reply_target,
                            "⚠️ Failed to persist queue mode.".to_string(),
                        )
                        .await;
                        return true;
                    }
                }

                let active_mode = current_queue_mode(session_store, &session_id);
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    format!(
                        "Queue mode for session `{}` is `{active_mode}`.",
                        short_session_id(&session_id)
                    ),
                )
                .await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to resolve active session for queue command ({}): {error}",
                    inbound_key
                );
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "⚠️ Failed to configure queue mode.".to_string(),
                )
                .await;
            }
        },
        SlashCommand::Subagents => {
            let agents = session_store
                .list_agents(COMMAND_LIST_LIMIT)
                .unwrap_or_default();
            let sessions = session_store
                .list_subagent_sessions(COMMAND_LIST_LIMIT)
                .unwrap_or_default();

            let mut text = String::new();
            let _ = writeln!(text, "Subagents");
            let _ = writeln!(text, "agents: {}", agents.len());
            for agent in agents.iter().take(5) {
                let _ = writeln!(text, "- {} ({})", agent.name, agent.agent_id);
            }
            let _ = writeln!(text, "sessions: {}", sessions.len());
            for session in sessions.iter().take(5) {
                let _ = writeln!(
                    text,
                    "- {} [{}]",
                    session.subagent_session_id, session.status
                );
            }

            send_command_response(target_channel, &msg.reply_target, text.trim().to_string()).await;
        }
        SlashCommand::Sessions => match session_store.get_or_create_active(inbound_key) {
            Ok(current_session_id) => {
                let sessions = session_store
                    .list_sessions(Some(inbound_key), COMMAND_LIST_LIMIT)
                    .unwrap_or_default();
                let mut text = String::new();
                let _ = writeln!(text, "Current session: `{}`", current_session_id.as_str());
                let _ = writeln!(text, "Sessions for key `{}`:", inbound_key);
                for session in sessions.iter().take(10) {
                    let marker = if session.session_id == current_session_id.as_str() {
                        "*"
                    } else {
                        "-"
                    };
                    let _ = writeln!(
                        text,
                        "{marker} {} status={} messages={}",
                        session.session_id, session.status, session.message_count
                    );
                }

                send_command_response(target_channel, &msg.reply_target, text.trim().to_string())
                    .await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to resolve active session for sessions command ({}): {error}",
                    inbound_key
                );
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "⚠️ Failed to list sessions.".to_string(),
                )
                .await;
            }
        },
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
                send_command_response(target_channel, &msg.reply_target, text.trim().to_string())
                    .await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to resolve session for /agents ({}): {error}",
                    inbound_key
                );
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "⚠️ Failed to list agents.".to_string(),
                )
                .await;
            }
        },
        SlashCommand::AgentSwitch { id_or_name } => {
            if id_or_name.is_empty() {
                send_command_response(
                    target_channel,
                    &msg.reply_target,
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
                                send_command_response(
                                    target_channel,
                                    &msg.reply_target,
                                    "⚠️ Failed to switch agent.".to_string(),
                                )
                                .await;
                                return true;
                            }
                            send_command_response(
                                target_channel,
                                &msg.reply_target,
                                format!(
                                    "Switched to agent `{}` ({}) for this session.",
                                    agent.name,
                                    &agent.agent_id[..agent.agent_id.len().min(8)]
                                ),
                            )
                            .await;
                        }
                        None => {
                            send_command_response(
                                target_channel,
                                &msg.reply_target,
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
                    send_command_response(
                        target_channel,
                        &msg.reply_target,
                        "⚠️ Failed to switch agent.".to_string(),
                    )
                    .await;
                }
            }
        }
        SlashCommand::Models => match session_store.get_or_create_active(inbound_key) {
            Ok(session_id) => {
                let override_val = session_store
                    .get_state_key(&session_id, SessionStore::MODEL_OVERRIDE_KEY)
                    .ok()
                    .flatten()
                    .and_then(|raw| {
                        serde_json::from_str::<String>(&raw)
                            .ok()
                            .or_else(|| (!raw.is_empty()).then(|| raw))
                    });
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "Model for session `{}`",
                    short_session_id(&session_id)
                );
                let _ = writeln!(
                    text,
                    "Override: {}",
                    override_val
                        .as_deref()
                        .unwrap_or("(none — use agent/default)")
                );
                let _ = writeln!(text, "Use /model <provider>/<model> to set override (e.g. openrouter/anthropic/claude-sonnet-4).");
                send_command_response(target_channel, &msg.reply_target, text.trim().to_string())
                    .await;
            }
            Err(error) => {
                tracing::error!(
                    "Failed to resolve session for /models ({}): {error}",
                    inbound_key
                );
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "⚠️ Failed to show model.".to_string(),
                )
                .await;
            }
        },
        SlashCommand::Model { provider_model } => {
            if provider_model.is_empty() {
                send_command_response(
                    target_channel,
                    &msg.reply_target,
                    "Usage: /model <provider>/<model>. Use /models to see current.".to_string(),
                )
                .await;
                return true;
            }
            match session_store.get_or_create_active(inbound_key) {
                Ok(session_id) => {
                    let value_json = serde_json::to_string(&provider_model.trim())
                        .unwrap_or_else(|_| format!("\"{}\"", provider_model.trim()));
                    if let Err(e) = session_store.set_state_key(
                        &session_id,
                        SessionStore::MODEL_OVERRIDE_KEY,
                        &value_json,
                    ) {
                        tracing::error!("Failed to set model_override: {e}");
                        send_command_response(
                            target_channel,
                            &msg.reply_target,
                            "⚠️ Failed to set model override.".to_string(),
                        )
                        .await;
                        return true;
                    }
                    send_command_response(
                        target_channel,
                        &msg.reply_target,
                        format!(
                            "Model override set to `{}` for this session.",
                            provider_model.trim()
                        ),
                    )
                    .await;
                }
                Err(error) => {
                    tracing::error!(
                        "Failed to resolve session for /model ({}): {error}",
                        inbound_key
                    );
                    send_command_response(
                        target_channel,
                        &msg.reply_target,
                        "⚠️ Failed to set model.".to_string(),
                    )
                    .await;
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
        assert_eq!(
            parse_slash_command("/queue steer-merge"),
            Some(SlashCommand::Queue {
                mode: Some("steer-merge".to_string())
            })
        );
        assert_eq!(
            parse_slash_command("/queue"),
            Some(SlashCommand::Queue { mode: None })
        );
        assert_eq!(
            parse_slash_command("/subagents"),
            Some(SlashCommand::Subagents)
        );
        assert_eq!(
            parse_slash_command("/sessions"),
            Some(SlashCommand::Sessions)
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
        assert!(!is_agent_command(&SlashCommand::Queue { mode: None }));
        assert!(!is_agent_command(&SlashCommand::Sessions));
    }

    #[test]
    fn parse_slash_command_rejects_unsupported_or_partial_commands() {
        assert_eq!(parse_slash_command("hello"), None);
        assert_eq!(parse_slash_command("/new-session"), None);
        assert_eq!(parse_slash_command("/sessionss"), None);
        assert_eq!(parse_slash_command("/unknown"), None);
    }
}
