//! Slash-command parsing and handling. Lives in agent so command semantics
//! are owned by the agent layer; channels only dispatch and send replies.

use crate::channels::traits::ChannelMessage;
use crate::channels::{
    dispatch_outbound_message, outbound_key_from_parts, ChannelRuntimeContext, SendMessage,
};
use crate::multi_agent;
use crate::session::SessionId;
use chrono::{Datelike, NaiveDate, Utc};
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
    /// Usage/cost query: /usage [summary|daily|monthly] [args]. Default summary.
    Usage {
        /// summary | daily | monthly
        dimension: String,
        /// For daily: one optional YYYY-MM-DD. For monthly: optional year, optional month (1-12).
        args: Vec<String>,
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
        "/usage" => {
            let dimension = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("session")
                .to_lowercase();
            let args: Vec<String> = parts
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(SlashCommand::Usage { dimension, args })
        }
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

/// Build usage/cost reply for /usage slash command. Dimension: session | summary | daily | monthly.
fn format_usage_reply(
    tracker: &crate::cost::CostTracker,
    dimension: &str,
    args: Vec<String>,
) -> Result<String, String> {
    match dimension {
        "session" => {
            let summary = tracker.get_summary().map_err(|e| e.to_string())?;
            let mut text = String::new();
            let _ = writeln!(text, "Session cost: ${:.4}", summary.session_cost_usd);
            let _ = writeln!(text, "Tokens: {} ({} requests)", summary.total_tokens, summary.request_count);
            if !summary.by_model.is_empty() {
                for (model, stats) in &summary.by_model {
                    let _ = writeln!(text, "  {}  ${:.4}  {} tokens", model, stats.cost_usd, stats.total_tokens);
                }
            }
            Ok(text.trim().to_string())
        }
        "summary" => {
            let summary = tracker.get_summary().map_err(|e| e.to_string())?;
            let mut text = String::new();
            let _ = writeln!(text, "Cost (UTC) — Session ${:.4} · Daily ${:.4} · Monthly ${:.4}", summary.session_cost_usd, summary.daily_cost_usd, summary.monthly_cost_usd);
            let _ = writeln!(text, "Tokens: {} ({} requests)", summary.total_tokens, summary.request_count);
            if !summary.by_model.is_empty() {
                for (model, stats) in &summary.by_model {
                    let _ = writeln!(text, "  {}  ${:.4}  {} tokens", model, stats.cost_usd, stats.total_tokens);
                }
            }
            Ok(text.trim().to_string())
        }
        "daily" => {
            let day = match args.first().map(String::as_str) {
                None => Utc::now().date_naive(),
                Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("Invalid date (use YYYY-MM-DD): {s}"))?,
            };
            let cost = tracker.get_daily_cost(day).map_err(|e| e.to_string())?;
            Ok(format!("Daily cost (UTC) {}: ${:.4}", day, cost))
        }
        "monthly" => {
            let (y, m) = match (args.get(0), args.get(1)) {
                (None, None) => {
                    let now = Utc::now();
                    (now.year(), now.month())
                }
                (Some(yr), Some(mo)) => {
                    let y: i32 = yr.parse().map_err(|_| format!("Invalid year: {yr}"))?;
                    let m: u32 = mo.parse().map_err(|_| format!("Invalid month: {mo}"))?;
                    if !(1..=12).contains(&m) {
                        return Err(format!("Month must be 1–12, got {m}"));
                    }
                    (y, m)
                }
                (Some(yr), None) => (yr.parse().map_err(|_| format!("Invalid year: {yr}"))?, Utc::now().month()),
                (None, Some(mo)) => {
                    let m: u32 = mo.parse().map_err(|_| format!("Invalid month: {mo}"))?;
                    if !(1..=12).contains(&m) {
                        return Err(format!("Month must be 1–12, got {m}"));
                    }
                    (Utc::now().year(), m)
                }
            };
            let cost = tracker.get_monthly_cost(y, m).map_err(|e| e.to_string())?;
            Ok(format!("Monthly cost (UTC) {y}-{m:02}: ${cost:.4}"))
        }
        _ => Err(format!("Unknown dimension: {dimension}. Use: session, summary, daily, monthly")),
    }
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
        SlashCommand::Usage { dimension, args } => {
            let reply = match ctx.cost_tracker.as_ref() {
                None => "Cost tracking is disabled.".to_string(),
                Some(tracker) => match format_usage_reply(tracker, &dimension, args) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("Usage command error: {e}");
                        format!("⚠️ Usage: {e}")
                    }
                },
            };
            dispatch_reply(&msg, reply).await;
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
        assert_eq!(
            parse_slash_command("/usage"),
            Some(SlashCommand::Usage {
                dimension: "session".to_string(),
                args: vec![],
            })
        );
        assert_eq!(
            parse_slash_command("/usage summary"),
            Some(SlashCommand::Usage {
                dimension: "summary".to_string(),
                args: vec![],
            })
        );
        assert_eq!(
            parse_slash_command("/usage daily 2025-02-23"),
            Some(SlashCommand::Usage {
                dimension: "daily".to_string(),
                args: vec!["2025-02-23".to_string()],
            })
        );
        assert_eq!(
            parse_slash_command("/usage monthly 2025 2"),
            Some(SlashCommand::Usage {
                dimension: "monthly".to_string(),
                args: vec!["2025".to_string(), "2".to_string()],
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

    #[test]
    fn is_agent_command_returns_false_for_usage() {
        assert!(!is_agent_command(&SlashCommand::Usage {
            dimension: "session".to_string(),
            args: vec![],
        }));
    }
}
