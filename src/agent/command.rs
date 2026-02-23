//! Slash-command parsing and handling. Lives in agent so command semantics
//! are owned by the agent layer; channels only dispatch and send replies.

use crate::channels::traits::ChannelMessage;
use crate::channels::{
    dispatch_outbound_message, outbound_key_from_parts, ChannelRuntimeContext, SendMessage,
};
use crate::multi_agent;
use crate::session::SessionId;
use std::fmt::Write;

const COMMAND_LIST_LIMIT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Stop,
    /// New session; optional agent_id to bind the new session to that workspace agent.
    New {
        agent_id: Option<String>,
    },
    Compact,
    Agents,
    AgentSwitch {
        id_or_name: String,
        set_default: bool,
    },
    /// Setup workspace agent: /setup_agent <agent_id> [provider/model] [temperature]
    SetupAgent {
        agent_id: String,
        provider_model: Option<String>,
        /// Temperature as parsed string (e.g. "0.5"); parsed to f64 when handling.
        temperature_str: Option<String>,
    },
    Models,
    Model {
        provider_model: String,
    },
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
        "/new" => {
            let agent_id = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(SlashCommand::New { agent_id })
        }
        "/compact" => Some(SlashCommand::Compact),
        "/agents" => Some(SlashCommand::Agents),
        "/agent" => {
            let id_or_name = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_default();
            let set_default = parts
                .next()
                .map(str::trim)
                .map(|s| s.eq_ignore_ascii_case("default"))
                .unwrap_or(false);
            Some(SlashCommand::AgentSwitch {
                id_or_name,
                set_default,
            })
        }
        "/setup_agent" => {
            let agent_id = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let agent_id = agent_id?;
            let provider_model = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let temperature_str = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(SlashCommand::SetupAgent {
                agent_id,
                provider_model,
                temperature_str,
            })
        }
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
            | SlashCommand::Compact
            | SlashCommand::Models
            | SlashCommand::Model { .. }
            | SlashCommand::AgentSwitch { .. }
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
        | SlashCommand::Compact
        | SlashCommand::Models
        | SlashCommand::Model { .. }
        | SlashCommand::AgentSwitch { .. } => {
            unreachable!(
                "handle_slash_command only handles global commands; agent commands must be enqueued to agent"
            );
        }
        SlashCommand::New { agent_id } => {
            match session_store.create_new(inbound_key, agent_id.as_deref()) {
                Ok(session_id) => {
                    dispatch_reply(
                        &msg,
                        format!(
                            "New session started ({}). Use /agent <agent_id> to switch.",
                            short_session_id(&session_id)
                        ),
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!("Failed to create new session: {e}");
                    dispatch_reply(&msg, "⚠️ Failed to start new session.".to_string()).await;
                }
            }
            return true;
        }
        SlashCommand::Agents => match session_store.get_or_create_active(inbound_key) {
            Ok(session_id) => {
                let workspace_agents = match session_store.list_workspace_agent_ids() {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::error!("Failed to list workspace agents: {e}");
                        dispatch_reply(&msg, "⚠️ Failed to list agents.".to_string()).await;
                        return true;
                    }
                };
                let active_id = session_store
                    .effective_workspace_agent_id(&session_id)
                    .ok()
                    .unwrap_or_else(|| multi_agent::DEFAULT_AGENT_ID.to_string());
                let mut text = String::new();
                let _ = writeln!(text, "Agents (session `{}`)", short_session_id(&session_id));
                let _ = writeln!(text, "Active: {}", active_id);
                for agent_id in workspace_agents.iter().take(20) {
                    let marker = if active_id == *agent_id { "* " } else { "- " };
                    let _ = writeln!(text, "{marker}{agent_id}");
                }
                let _ = writeln!(text, "Use /agent <agent_id> [default] to switch. Use /setup_agent <agent_id> to create.");
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
        SlashCommand::SetupAgent {
            agent_id,
            provider_model,
            temperature_str,
        } => {
            let temperature = temperature_str
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok());
            match crate::channels::handle_setup_agent_command(
                ctx,
                &msg,
                inbound_key,
                &agent_id,
                provider_model.as_deref(),
                temperature,
            )
            .await
            {
                Ok(content) => dispatch_reply(&msg, content).await,
                Err(e) => {
                    tracing::error!("Failed to create agent: {e}");
                    dispatch_reply(&msg, format!("⚠️ Failed to create agent: {e}")).await;
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
        assert_eq!(
            parse_slash_command("/new"),
            Some(SlashCommand::New { agent_id: None })
        );
        assert_eq!(
            parse_slash_command("/new worker"),
            Some(SlashCommand::New {
                agent_id: Some("worker".to_string())
            })
        );
        assert_eq!(
            parse_slash_command("   /compact"),
            Some(SlashCommand::Compact)
        );
        assert_eq!(parse_slash_command("/agents"), Some(SlashCommand::Agents));
        assert_eq!(
            parse_slash_command("/agent coder"),
            Some(SlashCommand::AgentSwitch {
                id_or_name: "coder".to_string(),
                set_default: false,
            })
        );
        assert_eq!(
            parse_slash_command("/agent worker default"),
            Some(SlashCommand::AgentSwitch {
                id_or_name: "worker".to_string(),
                set_default: true,
            })
        );
        assert_eq!(
            parse_slash_command("/setup_agent mybot openai/gpt-4 0.5"),
            Some(SlashCommand::SetupAgent {
                agent_id: "mybot".to_string(),
                provider_model: Some("openai/gpt-4".to_string()),
                temperature_str: Some("0.5".to_string()),
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
        assert!(!is_agent_command(&SlashCommand::New { agent_id: None }));
        assert!(is_agent_command(&SlashCommand::Compact));
        assert!(is_agent_command(&SlashCommand::Models));
        assert!(is_agent_command(&SlashCommand::Model {
            provider_model: "p/m".to_string()
        }));
        assert!(is_agent_command(&SlashCommand::AgentSwitch {
            id_or_name: "worker".to_string(),
            set_default: false,
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
