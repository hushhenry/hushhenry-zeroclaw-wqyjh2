//! Channel apply/validate helpers for the channels-repair TUI.
//! Pure functions: take string params, validate (HTTP where needed), return config.

use crate::config::{ChannelsConfig, DiscordConfig, TelegramConfig};
use anyhow::Result;
use std::thread;

fn parse_allowed_users(s: &str) -> Vec<String> {
    if s.trim() == "*" {
        return vec!["*".into()];
    }
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Build TelegramConfig from token and allowed_users string. Validates token via Telegram API.
pub fn apply_telegram(token: &str, allowed_users_str: &str) -> Result<Option<TelegramConfig>> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Ok(None);
    }
    let token_clone = token.clone();
    let (ok, bot_name) = thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        let url = format!("https://api.telegram.org/bot{token_clone}/getMe");
        let resp = client.get(&url).send()?;
        let ok = resp.status().is_success();
        let data: serde_json::Value = resp.json().unwrap_or_default();
        let bot_name = data
            .get("result")
            .and_then(|r| r.get("username"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok::<_, reqwest::Error>((ok, bot_name))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("thread panicked"))??;
    if !ok {
        anyhow::bail!("Telegram token invalid or connection failed");
    }
    let _ = bot_name;
    let allowed_users = parse_allowed_users(allowed_users_str);
    Ok(Some(TelegramConfig {
        bot_token: token,
        allowed_users,
    }))
}

/// Build DiscordConfig from token, optional guild_id, and allowed_users string. Validates token.
pub fn apply_discord(
    token: &str,
    guild_id: &str,
    allowed_users_str: &str,
) -> Result<Option<DiscordConfig>> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Ok(None);
    }
    let token_clone = token.clone();
    let (ok, _bot_name) = thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        let resp = client
            .get("https://discord.com/api/v10/users/@me")
            .header("Authorization", format!("Bot {token_clone}"))
            .send()?;
        let ok = resp.status().is_success();
        let data: serde_json::Value = resp.json().unwrap_or_default();
        let bot_name = data
            .get("username")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok::<_, reqwest::Error>((ok, bot_name))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("thread panicked"))??;
    if !ok {
        anyhow::bail!("Discord token invalid or connection failed");
    }
    let allowed_users = if allowed_users_str.trim().is_empty() {
        vec![]
    } else {
        parse_allowed_users(allowed_users_str)
    };
    Ok(Some(DiscordConfig {
        bot_token: token,
        guild_id: if guild_id.trim().is_empty() {
            None
        } else {
            Some(guild_id.trim().to_string())
        },
        allowed_users,
        listen_to_bots: false,
        mention_only: false,
    }))
}

/// Channel list for TUI: (label, configured).
pub fn channel_list(config: &ChannelsConfig) -> Vec<(String, bool)> {
    vec![
        ("Telegram".into(), config.telegram.is_some()),
        ("Discord".into(), config.discord.is_some()),
        ("Slack".into(), config.slack.is_some()),
        ("iMessage".into(), config.imessage.is_some()),
        ("Matrix".into(), config.matrix.is_some()),
        ("WhatsApp".into(), config.whatsapp.is_some()),
        ("IRC".into(), config.irc.is_some()),
        ("Webhook".into(), config.webhook.is_some()),
        ("DingTalk".into(), config.dingtalk.is_some()),
        ("QQ Official".into(), config.qq.is_some()),
    ]
}
