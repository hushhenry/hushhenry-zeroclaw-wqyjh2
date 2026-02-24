//! Ratatui-based interactive onboard wizard.
//!
//! Re-entrant: loads existing config when present; only writes config and
//! persists workspace when the user actually changes something (dirty).
//! Provider step follows zeroai-proxy-style flow: provider list → API key → model select.
//! Memory is fixed to sqlite (no step). Channels list all options and allow inline config.

use crate::config::{ChannelsConfig, ComposioConfig, Config, MemoryConfig, SecretsConfig};
use crate::oauth::{
    OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthPrompt,
    is_oauth_only_provider, oauth_provider_for_id,
};
use crate::oauth::store::save_oauth_credentials;
use crate::onboard::channels::{self, channel_list};
use crate::onboard::wizard::{
    curated_models_for_provider, default_model_for_provider, memory_config_defaults_for_backend,
    persist_workspace_selection, scaffold_main_workspace, ProjectContext,
};
use crate::providers::{list_providers, ProviderInfo};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::fs;
use std::io::{self, stdout};
use std::path::PathBuf;
use std::process::Command;

const COLOR_CYAN: Color = Color::Rgb(137, 220, 235);
const COLOR_GREEN: Color = Color::Rgb(166, 227, 161);
const COLOR_YELLOW: Color = Color::Rgb(249, 226, 175);
const COLOR_GRAY: Color = Color::Rgb(108, 112, 134);

/// Wizard step index (0-based for screens). Memory is fixed to sqlite (no step).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Welcome = 0,
    Workspace,
    Provider,
    Channels,
    ToolMode,
    ProjectContext,
    ZeroAi,
    Summary,
}

/// Provider step sub-screens (zeroai-proxy style: list → API key / OAuth → model select).
#[derive(Clone, Debug)]
enum ProviderScreen {
    List,
    ApiKey,
    /// OAuth flow: auth_url/hint from callbacks, user_input for code, oauth_result when done.
    OAuth {
        provider_id: String,
        auth_info: Arc<Mutex<Option<OAuthAuthInfo>>>,
        prompt_result: Arc<Mutex<Option<String>>>,
        oauth_result: Arc<Mutex<Option<Result<OAuthCredentials>>>>,
        user_input: String,
        error: Option<String>,
    },
    ModelSelect,
}

impl PartialEq for ProviderScreen {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ProviderScreen::List, ProviderScreen::List) => true,
            (ProviderScreen::ApiKey, ProviderScreen::ApiKey) => true,
            (ProviderScreen::OAuth { provider_id: a, .. }, ProviderScreen::OAuth { provider_id: b, .. }) => a == b,
            (ProviderScreen::ModelSelect, ProviderScreen::ModelSelect) => true,
            _ => false,
        }
    }
}
impl Eq for ProviderScreen {}

/// OAuth callbacks for TUI: pass auth URL and prompt result via shared state.
struct TuiOAuthCallbacks {
    auth_info: Arc<Mutex<Option<OAuthAuthInfo>>>,
    prompt_result: Arc<Mutex<Option<String>>>,
    progress: Arc<Mutex<String>>,
}

#[async_trait]
impl OAuthCallbacks for TuiOAuthCallbacks {
    fn on_auth(&self, info: OAuthAuthInfo) {
        if let Ok(mut guard) = self.auth_info.lock() {
            *guard = Some(info);
        }
    }

    async fn on_prompt(&self, _prompt: OAuthPrompt) -> anyhow::Result<String> {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(mut guard) = self.prompt_result.lock() {
                if let Some(val) = guard.take() {
                    return Ok(val);
                }
            }
        }
    }

    fn on_progress(&self, message: &str) {
        if let Ok(mut guard) = self.progress.lock() {
            *guard = message.to_string();
        }
    }
}

/// Channels step sub-screens: list of channels or inline config for one.
#[derive(Clone, Debug)]
enum ChannelsScreen {
    List,
    Input {
        channel_index: usize,
        fields: Vec<String>,
        focused: usize,
        cursor_pos: usize,
        error: Option<String>,
    },
}

/// Tool mode choice: Sovereign (local) or Composio (OAuth).
const TOOL_MODE_OPTIONS: &[(bool, &str)] = &[
    (false, "Sovereign (local) — no Composio, workspace tools only"),
    (true, "Composio (OAuth) — 1000+ OAuth tools via Composio"),
];

impl Step {
    const ALL: &'static [Step] = &[
        Step::Welcome,
        Step::Workspace,
        Step::Provider,
        Step::Channels,
        Step::ToolMode,
        Step::ProjectContext,
        Step::ZeroAi,
        Step::Summary,
    ];

    fn title(&self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Workspace => "Workspace",
            Step::Provider => "AI Provider & API Key",
            Step::Channels => "Channels",
            Step::ToolMode => "Tool Mode & Security",
            Step::ProjectContext => "Project Context",
            Step::ZeroAi => "AI Providers (ZeroAI)",
            Step::Summary => "Summary",
        }
    }

    fn next(self) -> Option<Step> {
        let i = Step::ALL.iter().position(|&s| s == self)?;
        Step::ALL.get(i + 1).copied()
    }

    fn prev(self) -> Option<Step> {
        let i = Step::ALL.iter().position(|&s| s == self)?;
        if i == 0 {
            None
        } else {
            Step::ALL.get(i - 1).copied()
        }
    }
}

/// Tracks what was modified so we only save when dirty.
#[derive(Default)]
pub struct DirtyFlags {
    pub workspace: bool,
    pub provider: bool,
    pub channels: bool,
    pub tool_mode: bool,
    pub memory: bool,
    pub project_context: bool,
    pub zeroai: bool,
}

impl DirtyFlags {
    pub fn any(&self) -> bool {
        self.workspace
            || self.provider
            || self.channels
            || self.tool_mode
            || self.memory
            || self.project_context
            || self.zeroai
    }
}

/// In-memory wizard state (before building Config).
pub struct WizardState {
    pub workspace_dir: PathBuf,
    pub config_path: PathBuf,
    pub provider: String,
    pub api_key: String,
    pub api_url: Option<String>,
    pub model: String,
    pub channels_config: ChannelsConfig,
    pub composio_config: ComposioConfig,
    pub secrets_config: SecretsConfig,
    pub memory_config: MemoryConfig,
    pub project_ctx: ProjectContext,
    pub dirty: DirtyFlags,
    /// If true, we loaded existing config (re-entrant); only save when dirty.
    pub reentrant: bool,
}

impl Default for WizardState {
    fn default() -> Self {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let zeroclaw_dir = home.join(".zeroclaw");
        let workspace_dir = zeroclaw_dir.join("workspace");
        let config_path = zeroclaw_dir.join("config.toml");
        Self {
            workspace_dir,
            config_path,
            provider: "openrouter".to_string(),
            api_key: String::new(),
            api_url: None,
            model: default_model_for_provider("openrouter"),
            channels_config: ChannelsConfig::default(),
            composio_config: ComposioConfig::default(),
            secrets_config: SecretsConfig::default(),
            memory_config: memory_config_defaults_for_backend("sqlite"),
            project_ctx: ProjectContext::default(),
            dirty: DirtyFlags::default(),
            reentrant: false,
        }
    }
}

impl WizardState {
    /// Build Config from current state (for save/summary).
    pub fn build_config(&self) -> Config {
        Config {
            workspace_dir: self.workspace_dir.clone(),
            config_path: self.config_path.clone(),
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            api_url: self.api_url.clone(),
            default_provider: Some(self.provider.clone()),
            default_model: Some(self.model.clone()),
            default_temperature: 0.7,
            observability: crate::config::ObservabilityConfig::default(),
            autonomy: crate::config::AutonomyConfig::default(),
            runtime: crate::config::RuntimeConfig::default(),
            reliability: crate::config::ReliabilityConfig::default(),
            scheduler: crate::config::schema::SchedulerConfig::default(),
            agent: crate::config::schema::AgentConfig::default(),
            provider_groups: Vec::new(),
            cron: crate::config::CronConfig::default(),
            channels_config: self.channels_config.clone(),
            memory: self.memory_config.clone(),
            session: crate::config::SessionConfig::default(),
            gateway: crate::config::GatewayConfig::default(),
            composio: self.composio_config.clone(),
            secrets: self.secrets_config.clone(),
            browser: crate::config::BrowserConfig::default(),
            http_request: crate::config::HttpRequestConfig::default(),
            identity: crate::config::IdentityConfig::default(),
            cost: crate::config::CostConfig::default(),
        }
    }

    /// Load state from existing config (re-entrant).
    pub fn from_config(config: &Config) -> Self {
        let mut state = Self::default();
        state.workspace_dir = config.workspace_dir.clone();
        state.config_path = config.config_path.clone();
        state.provider = config
            .default_provider
            .clone()
            .unwrap_or_else(|| "openrouter".into());
        state.api_key = config.api_key.clone().unwrap_or_default();
        state.api_url = config.api_url.clone();
        state.model = config
            .default_model
            .clone()
            .unwrap_or_else(|| "anthropic/claude-sonnet-4.5".into());
        state.channels_config = config.channels_config.clone();
        state.composio_config = config.composio.clone();
        state.secrets_config = config.secrets.clone();
        state.memory_config = config.memory.clone();
        state.project_ctx = ProjectContext {
            user_name: std::env::var("USER").unwrap_or_else(|_| "User".into()),
            timezone: "UTC".into(),
            agent_name: "ZeroClaw".into(),
            communication_style: "Be warm, natural, and clear. Use occasional relevant emojis (1-2 max) and avoid robotic phrasing.".into(),
        };
        state.reentrant = true;
        state
    }
}

/// Run the full onboard wizard in TUI. Re-entrant: if config exists, loads it and only saves when dirty.
pub fn run_wizard_tui() -> Result<Config> {
    let existing = Config::load_if_exists();
    let mut state = existing
        .as_ref()
        .map(WizardState::from_config)
        .unwrap_or_default();

    enable_raw_mode().context("enable_raw_mode")?;
    stdout()
        .execute(EnterAlternateScreen)
        .context("EnterAlternateScreen")?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("Terminal::new")?;

    let result = run_tui_loop(&mut terminal, &mut state);

    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);

    match result {
        Ok(Some(config)) => Ok(config),
        Ok(None) => {
            if state.reentrant {
                existing.ok_or_else(|| anyhow::anyhow!("Onboard cancelled"))
            } else {
                anyhow::bail!("Onboard cancelled")
            }
        }
        Err(e) => Err(e),
    }
}

// ── Channels repair TUI ─────────────────────────────────────────────

const CHANNELS_REPAIR_ITEMS: usize = 11; // 10 channels + Done

#[derive(Clone)]
enum ChannelsRepairScreen {
    List,
    Input {
        channel_index: usize,
        fields: Vec<String>,
        focused: usize,
        cursor_pos: usize,
        error: Option<String>,
    },
}

/// Run channels repair in TUI. Only saves when config is modified.
pub fn run_channels_repair_tui() -> Result<Config> {
    let mut config = Config::load_or_init()?;

    enable_raw_mode().context("enable_raw_mode")?;
    stdout()
        .execute(EnterAlternateScreen)
        .context("EnterAlternateScreen")?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("Terminal::new")?;

    let mut list_state = ListState::default();
    list_state.select(Some(0));
    let mut screen = ChannelsRepairScreen::List;
    let mut dirty = false;

    loop {
        terminal.draw(|f| draw_channels_repair(f, &config, &screen, &list_state))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }

                match &mut screen {
                    ChannelsRepairScreen::List => {
                        let items = CHANNELS_REPAIR_ITEMS;
                        let i = list_state.selected().unwrap_or(0);
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Up | KeyCode::Char('k') => {
                                list_state.select(Some(if i == 0 { items - 1 } else { i - 1 }));
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                list_state.select(Some((i + 1) % items));
                            }
                            KeyCode::Enter => {
                                if i == 10 {
                                    // Done
                                    if dirty {
                                        config.save()?;
                                        crate::onboard::wizard::persist_workspace_selection(
                                            &config.config_path,
                                        )?;
                                    }
                                    let _ = disable_raw_mode();
                                    let _ = stdout().execute(LeaveAlternateScreen);
                                    return Ok(config);
                                }
                                if i == 0 {
                                    // Telegram
                                    let token = config
                                        .channels_config
                                        .telegram
                                        .as_ref()
                                        .map(|t| t.bot_token.clone())
                                        .unwrap_or_default();
                                    let allowed = config
                                        .channels_config
                                        .telegram
                                        .as_ref()
                                        .map(|t| t.allowed_users.join(", "))
                                        .unwrap_or_else(|| "*".into());
                                    screen = ChannelsRepairScreen::Input {
                                        channel_index: 0,
                                        fields: vec![token, allowed],
                                        focused: 0,
                                        cursor_pos: 0,
                                        error: None,
                                    };
                                } else if i == 1 {
                                    // Discord
                                    let (token, guild, allowed) = config
                                        .channels_config
                                        .discord
                                        .as_ref()
                                        .map(|d| {
                                            (
                                                d.bot_token.clone(),
                                                d.guild_id.clone().unwrap_or_default(),
                                                d.allowed_users.join(", "),
                                            )
                                        })
                                        .unwrap_or_else(|| {
                                            (String::new(), String::new(), String::new())
                                        });
                                    screen = ChannelsRepairScreen::Input {
                                        channel_index: 1,
                                        fields: vec![token, guild, allowed],
                                        focused: 0,
                                        cursor_pos: 0,
                                        error: None,
                                    };
                                }
                            }
                            _ => {}
                        }
                    }
                    ChannelsRepairScreen::Input {
                        channel_index,
                        fields,
                        focused,
                        cursor_pos,
                        error,
                    } => match key.code {
                        KeyCode::Esc => {
                            screen = ChannelsRepairScreen::List;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            *error = None;
                            if *focused > 0 {
                                *focused -= 1;
                                *cursor_pos = fields[*focused].len().min(*cursor_pos);
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            *error = None;
                            if *focused + 1 < fields.len() {
                                *focused += 1;
                                *cursor_pos = fields[*focused].len().min(*cursor_pos);
                            }
                        }
                        KeyCode::Enter => {
                            *error = None;
                            if *channel_index == 0 {
                                match channels::apply_telegram(
                                    &fields[0],
                                    fields.get(1).map(|s| s.as_str()).unwrap_or(""),
                                ) {
                                    Ok(Some(tc)) => {
                                        config.channels_config.telegram = Some(tc);
                                        dirty = true;
                                        screen = ChannelsRepairScreen::List;
                                    }
                                    Ok(None) => {
                                        config.channels_config.telegram = None;
                                        dirty = true;
                                        screen = ChannelsRepairScreen::List;
                                    }
                                    Err(e) => *error = Some(e.to_string()),
                                }
                            } else if *channel_index == 1 {
                                match channels::apply_discord(
                                    &fields[0],
                                    fields.get(1).map(|s| s.as_str()).unwrap_or(""),
                                    fields.get(2).map(|s| s.as_str()).unwrap_or(""),
                                ) {
                                    Ok(Some(dc)) => {
                                        config.channels_config.discord = Some(dc);
                                        dirty = true;
                                        screen = ChannelsRepairScreen::List;
                                    }
                                    Ok(None) => {
                                        config.channels_config.discord = None;
                                        dirty = true;
                                        screen = ChannelsRepairScreen::List;
                                    }
                                    Err(e) => *error = Some(e.to_string()),
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            *error = None;
                            if *cursor_pos > 0 {
                                fields[*focused].remove(*cursor_pos - 1);
                                *cursor_pos -= 1;
                            }
                        }
                        KeyCode::Char(c) => {
                            *error = None;
                            fields[*focused].insert(*cursor_pos, c);
                            *cursor_pos += 1;
                        }
                        KeyCode::Left => {
                            if *cursor_pos > 0 {
                                *cursor_pos -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if *cursor_pos < fields[*focused].len() {
                                *cursor_pos += 1;
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }

    let _ = disable_raw_mode();
    let _ = stdout().execute(LeaveAlternateScreen);
    Ok(config)
}

fn draw_channels_repair(
    f: &mut Frame,
    config: &Config,
    screen: &ChannelsRepairScreen,
    list_state: &ListState,
) {
    let area = f.area();
    let chunks = Layout::default()
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Channels Repair ",
            Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "— update tokens and allowlists",
            Style::default().fg(COLOR_GRAY),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(COLOR_GRAY)),
    );
    f.render_widget(title, chunks[0]);

    match screen {
        ChannelsRepairScreen::List => {
            let list_items = channel_list(&config.channels_config);
            let items: Vec<ratatui::text::Line> = list_items
                .iter()
                .enumerate()
                .map(|(_i, (label, configured))| {
                    let status = if *configured { "✅" } else { "—" };
                    Line::from(format!(
                        "{}  {} {}",
                        status,
                        label,
                        if *configured { "connected" } else { "" }
                    ))
                })
                .chain(std::iter::once(Line::from(Span::styled(
                    "Done — save and exit",
                    Style::default().fg(COLOR_GREEN),
                ))))
                .collect();
            let list = ratatui::widgets::List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Select channel (Enter to edit)")
                        .border_style(Style::default().fg(COLOR_GRAY)),
                )
                .highlight_style(Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD))
                .highlight_symbol("> ");
            let mut state = list_state.clone();
            f.render_stateful_widget(list, chunks[1], &mut state);
        }
        ChannelsRepairScreen::Input {
            channel_index,
            fields,
            focused,
            cursor_pos: _,
            error,
        } => {
            let labels: Vec<&str> = if *channel_index == 0 {
                vec![
                    "Bot token (from @BotFather):",
                    "Allowed users (comma or *):",
                ]
            } else {
                vec![
                    "Bot token:",
                    "Guild ID (optional):",
                    "Allowed user IDs (comma or *):",
                ]
            };
            let mut lines = Vec::new();
            for (i, (label, value)) in labels.iter().zip(fields.iter()).enumerate() {
                let display: String = value.chars().take(60).collect();
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", label), Style::default().fg(COLOR_GRAY)),
                    Span::raw(display),
                    Span::styled(
                        if i == *focused { " █" } else { "" },
                        Style::default().fg(COLOR_CYAN),
                    ),
                ]));
            }
            if let Some(err) = error {
                lines.push(Line::from(Span::styled(
                    format!("Error: {}", err),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter = save   Esc = back",
                Style::default().fg(COLOR_GRAY),
            )));
            let block = Block::default()
                .borders(Borders::ALL)
                .title(if *channel_index == 0 {
                    "Telegram"
                } else {
                    "Discord"
                })
                .border_style(Style::default().fg(COLOR_GRAY));
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
                chunks[1],
            );
        }
    }

    let hint = Paragraph::new(Line::from(Span::styled(
        " ↑↓ select   Enter edit/done   Esc back   q quit",
        Style::default().fg(COLOR_GRAY),
    )))
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(hint, chunks[2]);
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut WizardState,
) -> Result<Option<Config>> {
    let mut step = Step::Welcome;
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    // Provider step sub-state (zeroai-proxy style)
    let mut provider_screen = ProviderScreen::List;
    let mut provider_list_state = ListState::default();
    let mut model_list_state = ListState::default();
    let providers = list_providers();
    if !providers.is_empty() {
        provider_list_state.select(Some(0));
    }

    // Channels step sub-state
    let mut channels_screen = ChannelsScreen::List;
    let mut channel_list_state = ListState::default();
    channel_list_state.select(Some(0));

    // Tool mode: 0 = Sovereign, 1 = Composio
    let mut tool_mode_index = if state.composio_config.enabled { 1 } else { 0 };
    let mut tool_mode_list_state = ListState::default();
    tool_mode_list_state.select(Some(tool_mode_index));

    // Project context: which field is focused (0..4)
    let mut project_focused = 0u8;

    loop {
        terminal.draw(|f| {
            draw(
                f,
                state,
                step,
                &list_state,
                &provider_screen,
                &providers,
                &mut provider_list_state,
                &mut model_list_state,
                &channels_screen,
                &mut channel_list_state,
                &mut tool_mode_list_state,
                project_focused,
            )
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }

                match step {
                    Step::Welcome => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                        KeyCode::Enter | KeyCode::Down => step = Step::Workspace,
                        _ => {}
                    },
                    Step::Workspace => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                        KeyCode::Backspace => step = Step::Welcome,
                        KeyCode::Enter | KeyCode::Char('n') => {
                            step = Step::Provider;
                            provider_screen = ProviderScreen::List;
                            if !providers.is_empty() {
                                provider_list_state.select(Some(0));
                            }
                        }
                        _ => {}
                    },
                    Step::Provider => {
                        handle_provider_step(
                            key,
                            state,
                            &mut step,
                            &mut provider_screen,
                            &providers,
                            &mut provider_list_state,
                            &mut model_list_state,
                        );
                    }
                    Step::Channels => {
                        handle_channels_step(
                            key,
                            state,
                            &mut step,
                            &mut channels_screen,
                            &mut channel_list_state,
                        );
                    }
                    Step::ToolMode => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                step = step.prev().unwrap_or(Step::Welcome);
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                tool_mode_index = if tool_mode_index == 0 { 1 } else { 0 };
                                tool_mode_list_state.select(Some(tool_mode_index));
                                state.composio_config.enabled = TOOL_MODE_OPTIONS[tool_mode_index].0;
                                state.dirty.tool_mode = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                tool_mode_index = if tool_mode_index == 1 { 0 } else { 1 };
                                tool_mode_list_state.select(Some(tool_mode_index));
                                state.composio_config.enabled = TOOL_MODE_OPTIONS[tool_mode_index].0;
                                state.dirty.tool_mode = true;
                            }
                            KeyCode::Enter | KeyCode::Char('n') => {
                                step = step.next().unwrap_or(step);
                            }
                            KeyCode::Backspace => {
                                if let Some(prev) = step.prev() {
                                    step = prev;
                                }
                            }
                            _ => {}
                        }
                    }
                    Step::ProjectContext => {
                        handle_project_context_step(key, state, &mut step, &mut project_focused);
                    }
                    Step::ZeroAi => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            step = step.prev().unwrap_or(Step::Welcome);
                        }
                        KeyCode::Char('r') => {
                            let _ = disable_raw_mode();
                            let _ = stdout().execute(LeaveAlternateScreen);
                            run_zeroai_proxy_config();
                            let _ = enable_raw_mode();
                            let _ = stdout().execute(EnterAlternateScreen);
                            state.dirty.zeroai = true;
                        }
                        KeyCode::Enter | KeyCode::Char('n') => {
                            step = step.next().unwrap_or(step);
                        }
                        KeyCode::Backspace => {
                            if let Some(prev) = step.prev() {
                                step = prev;
                            }
                        }
                        _ => {}
                    },
                    Step::Summary => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => step = Step::ZeroAi,
                        KeyCode::Enter => {
                            let should_save = state.dirty.any() || !state.reentrant;
                            if should_save {
                                state.build_config().save()?;
                                persist_workspace_selection(&state.config_path)?;
                                if state.dirty.workspace
                                    || state.dirty.project_context
                                    || !state.reentrant
                                {
                                    fs::create_dir_all(&state.workspace_dir)
                                        .context("create workspace dir")?;
                                    scaffold_main_workspace(
                                        &state.workspace_dir,
                                        &state.project_ctx,
                                    )?;
                                }
                                return Ok(Some(state.build_config()));
                            }
                            if state.reentrant {
                                return Ok(Some(state.build_config()));
                            }
                            return Ok(None);
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

fn handle_provider_step(
    key: crossterm::event::KeyEvent,
    state: &mut WizardState,
    step: &mut Step,
    provider_screen: &mut ProviderScreen,
    providers: &[ProviderInfo],
    provider_list_state: &mut ListState,
    model_list_state: &mut ListState,
) {
    use crossterm::event::KeyCode;

    // If we're in OAuth and result just arrived, handle it once
    let oauth_done_result = if let ProviderScreen::OAuth {
        provider_id,
        oauth_result,
        error,
        ..
    } = provider_screen
    {
        let taken = {
            let mut guard = match oauth_result.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.take()
        };
        match taken {
            Some(Ok(creds)) => {
                let pid = provider_id.clone();
                if let Err(e) = save_oauth_credentials(&state.config_path, &pid, &creds) {
                    *error = Some(e.to_string());
                    None
                } else if let Some(provider) = oauth_provider_for_id(&pid) {
                    state.api_key = provider.get_api_key(&creds);
                    state.dirty.provider = true;
                    let models = curated_models_for_provider(&state.provider);
                    let idx = models
                        .iter()
                        .position(|(id, _)| id == &state.model)
                        .unwrap_or(0);
                    model_list_state.select(Some(idx));
                    Some(())
                } else {
                    None
                }
            }
            Some(Err(e)) => {
                *error = Some(e.to_string());
                None
            }
            None => None,
        }
    } else {
        None
    };
    if oauth_done_result.is_some() {
        *provider_screen = ProviderScreen::ModelSelect;
    }

    match provider_screen {
        ProviderScreen::List => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => *step = Step::Workspace,
            KeyCode::Up | KeyCode::Char('k') => {
                let i = provider_list_state.selected().unwrap_or(0);
                let next = if i == 0 {
                    providers.len().saturating_sub(1)
                } else {
                    i - 1
                };
                provider_list_state.select(Some(next));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = provider_list_state.selected().unwrap_or(0);
                let next = if i + 1 >= providers.len() { 0 } else { i + 1 };
                provider_list_state.select(Some(next));
            }
            KeyCode::Enter => {
                if let Some(idx) = provider_list_state.selected() {
                    if idx < providers.len() {
                        let p = &providers[idx];
                        state.provider = p.name.to_string();
                        state.model = default_model_for_provider(p.name);
                        state.dirty.provider = true;
                        let provider_id = state.provider.clone();
                        if is_oauth_only_provider(&provider_id) {
                            let auth_info = Arc::new(Mutex::new(None));
                            let prompt_result = Arc::new(Mutex::new(None));
                            let oauth_result = Arc::new(Mutex::new(None));
                            let or_clone = oauth_result.clone();
                            let auth_info_c = auth_info.clone();
                            let prompt_result_c = prompt_result.clone();
                            let provider_id_for_thread = provider_id.clone();
                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().unwrap();
                                let callbacks = TuiOAuthCallbacks {
                                    auth_info: auth_info_c,
                                    prompt_result: prompt_result_c,
                                    progress: Arc::new(Mutex::new(String::new())),
                                };
                                let result = rt.block_on(async {
                                    match oauth_provider_for_id(&provider_id_for_thread) {
                                        Some(p) => p.login(&callbacks).await,
                                        None => anyhow::bail!("unknown oauth provider"),
                                    }
                                });
                                let _ = or_clone.lock().unwrap().insert(result);
                            });
                            *provider_screen = ProviderScreen::OAuth {
                                provider_id,
                                auth_info,
                                prompt_result,
                                oauth_result,
                                user_input: String::new(),
                                error: None,
                            };
                        } else {
                            *provider_screen = ProviderScreen::ApiKey;
                        }
                    }
                }
            }
            KeyCode::Backspace => *step = Step::Workspace,
            _ => {}
        },
        ProviderScreen::ApiKey => match key.code {
            KeyCode::Esc => *provider_screen = ProviderScreen::List,
            KeyCode::Backspace => {
                if !state.api_key.is_empty() {
                    state.api_key.pop();
                    state.dirty.provider = true;
                }
            }
            KeyCode::Enter => {
                state.dirty.provider = true;
                let models = curated_models_for_provider(&state.provider);
                let idx = models
                    .iter()
                    .position(|(id, _)| id == &state.model)
                    .unwrap_or(0);
                model_list_state.select(Some(idx));
                *provider_screen = ProviderScreen::ModelSelect;
            }
            KeyCode::Char(c) => {
                state.api_key.push(c);
                state.dirty.provider = true;
            }
            _ => {}
        },
        ProviderScreen::OAuth {
            provider_id: _,
            prompt_result,
            user_input,
            error,
            ..
        } => match key.code {
            KeyCode::Esc => *provider_screen = ProviderScreen::List,
            KeyCode::Backspace => {
                user_input.pop();
                *error = None;
            }
            KeyCode::Enter => {
                *error = None;
                let input = user_input.trim().to_string();
                if !input.is_empty() {
                    if let Ok(mut guard) = prompt_result.lock() {
                        *guard = Some(input);
                    }
                    user_input.clear();
                }
            }
            KeyCode::Char(c) => {
                user_input.push(c);
                *error = None;
            }
            _ => {}
        },
        ProviderScreen::ModelSelect => {
            let models = curated_models_for_provider(&state.provider);
            match key.code {
                KeyCode::Esc => *provider_screen = ProviderScreen::ApiKey,
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = model_list_state.selected().unwrap_or(0);
                    let next = if i == 0 {
                        models.len().saturating_sub(1)
                    } else {
                        i - 1
                    };
                    model_list_state.select(Some(next));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = model_list_state.selected().unwrap_or(0);
                    let next = if i + 1 >= models.len() { 0 } else { i + 1 };
                    model_list_state.select(Some(next));
                }
                KeyCode::Enter => {
                    if let Some(idx) = model_list_state.selected() {
                        if idx < models.len() {
                            state.model = models[idx].0.clone();
                            state.dirty.provider = true;
                        }
                    }
                    *step = step.next().unwrap_or(*step);
                    *provider_screen = ProviderScreen::List;
                }
                KeyCode::Backspace => *provider_screen = ProviderScreen::ApiKey,
                _ => {}
            }
        }
    }
}

fn handle_channels_step(
    key: crossterm::event::KeyEvent,
    state: &mut WizardState,
    step: &mut Step,
    channels_screen: &mut ChannelsScreen,
    channel_list_state: &mut ListState,
) {
    use crossterm::event::KeyCode;
    let list_len = channel_list(&state.channels_config).len() + 1; // + Done

    match channels_screen {
        ChannelsScreen::List => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => *step = step.prev().unwrap_or(Step::Welcome),
            KeyCode::Up | KeyCode::Char('k') => {
                let i = channel_list_state.selected().unwrap_or(0);
                channel_list_state.select(Some(if i == 0 { list_len - 1 } else { i - 1 }));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = channel_list_state.selected().unwrap_or(0);
                channel_list_state.select(Some((i + 1) % list_len));
            }
            KeyCode::Enter => {
                let i = channel_list_state.selected().unwrap_or(0);
                if i == list_len - 1 {
                    *step = step.next().unwrap_or(*step);
                    return;
                }
                if i == 0 {
                    let token = state
                        .channels_config
                        .telegram
                        .as_ref()
                        .map(|t| t.bot_token.clone())
                        .unwrap_or_default();
                    let allowed = state
                        .channels_config
                        .telegram
                        .as_ref()
                        .map(|t| t.allowed_users.join(", "))
                        .unwrap_or_else(|| "*".into());
                    *channels_screen = ChannelsScreen::Input {
                        channel_index: 0,
                        fields: vec![token, allowed],
                        focused: 0,
                        cursor_pos: 0,
                        error: None,
                    };
                } else if i == 1 {
                    let (token, guild, allowed) = state
                        .channels_config
                        .discord
                        .as_ref()
                        .map(|d| {
                            (
                                d.bot_token.clone(),
                                d.guild_id.clone().unwrap_or_default(),
                                d.allowed_users.join(", "),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), String::new(), String::new()));
                    *channels_screen = ChannelsScreen::Input {
                        channel_index: 1,
                        fields: vec![token, guild, allowed],
                        focused: 0,
                        cursor_pos: 0,
                        error: None,
                    };
                }
            }
            KeyCode::Backspace => *step = step.prev().unwrap_or(Step::Welcome),
            _ => {}
        },
        ChannelsScreen::Input {
            channel_index,
            fields,
            focused,
            cursor_pos,
            error,
        } => match key.code {
            KeyCode::Esc => *channels_screen = ChannelsScreen::List,
            KeyCode::Up | KeyCode::Char('k') => {
                *error = None;
                if *focused > 0 {
                    *focused -= 1;
                    *cursor_pos = fields[*focused].len().min(*cursor_pos);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *error = None;
                if *focused + 1 < fields.len() {
                    *focused += 1;
                    *cursor_pos = fields[*focused].len().min(*cursor_pos);
                }
            }
            KeyCode::Backspace => {
                *error = None;
                if *cursor_pos > 0 {
                    fields[*focused].remove(*cursor_pos - 1);
                    *cursor_pos -= 1;
                }
            }
            KeyCode::Char(c) => {
                *error = None;
                fields[*focused].insert(*cursor_pos, c);
                *cursor_pos += 1;
            }
            KeyCode::Left => {
                if *cursor_pos > 0 {
                    *cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if *cursor_pos < fields[*focused].len() {
                    *cursor_pos += 1;
                }
            }
            KeyCode::Enter => {
                *error = None;
                if *channel_index == 0 {
                    match channels::apply_telegram(
                        &fields[0],
                        fields.get(1).map(|s| s.as_str()).unwrap_or(""),
                    ) {
                        Ok(Some(tc)) => {
                            state.channels_config.telegram = Some(tc);
                            state.dirty.channels = true;
                            *channels_screen = ChannelsScreen::List;
                        }
                        Ok(None) => {
                            state.channels_config.telegram = None;
                            state.dirty.channels = true;
                            *channels_screen = ChannelsScreen::List;
                        }
                        Err(e) => *error = Some(e.to_string()),
                    }
                } else if *channel_index == 1 {
                    match channels::apply_discord(
                        &fields[0],
                        fields.get(1).map(|s| s.as_str()).unwrap_or(""),
                        fields.get(2).map(|s| s.as_str()).unwrap_or(""),
                    ) {
                        Ok(Some(dc)) => {
                            state.channels_config.discord = Some(dc);
                            state.dirty.channels = true;
                            *channels_screen = ChannelsScreen::List;
                        }
                        Ok(None) => {
                            state.channels_config.discord = None;
                            state.dirty.channels = true;
                            *channels_screen = ChannelsScreen::List;
                        }
                        Err(e) => *error = Some(e.to_string()),
                    }
                }
            }
            _ => {}
        },
    }
}

fn handle_project_context_step(
    key: crossterm::event::KeyEvent,
    state: &mut WizardState,
    step: &mut Step,
    project_focused: &mut u8,
) {
    use crossterm::event::KeyCode;
    let ctx = &mut state.project_ctx;
    let f = *project_focused as usize;
    let fields: &mut [&mut String] = &mut [
        &mut ctx.user_name,
        &mut ctx.timezone,
        &mut ctx.agent_name,
        &mut ctx.communication_style,
    ];

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => *step = step.prev().unwrap_or(Step::Welcome),
        KeyCode::Enter if *project_focused == 3 => {
            *step = step.next().unwrap_or(*step);
        }
        KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => {
            *project_focused = (*project_focused + 1) % 4;
            state.dirty.project_context = true;
        }
        KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
            *project_focused = if *project_focused == 0 { 3 } else { *project_focused - 1 };
            state.dirty.project_context = true;
        }
        KeyCode::Backspace => {
            if f < fields.len() && !fields[f].is_empty() {
                fields[f].pop();
                state.dirty.project_context = true;
            }
        }
        KeyCode::Char(c) => {
            if f < fields.len() {
                fields[f].push(c);
                state.dirty.project_context = true;
            }
        }
        _ => {}
    }
}

/// Run ZeroAI TUI config (zeroai-proxy config). Blocks until user exits.
fn run_zeroai_proxy_config() {
    let status = Command::new("zeroai-proxy").arg("config").status();
    match status {
        Ok(s) if s.success() => {}
        Ok(_) => {}
        Err(_) => {
            eprintln!(
                "zeroclaw: zeroai-proxy not found. Install from https://github.com/hushhenry/zeroai and ensure zeroai-proxy is on PATH."
            );
            eprintln!("Press Enter to continue.");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    }
}

fn draw(
    f: &mut Frame,
    state: &WizardState,
    step: Step,
    _list_state: &ListState,
    provider_screen: &ProviderScreen,
    providers: &[ProviderInfo],
    provider_list_state: &mut ListState,
    model_list_state: &mut ListState,
    channels_screen: &ChannelsScreen,
    channel_list_state: &mut ListState,
    tool_mode_list_state: &mut ListState,
    project_focused: u8,
) {
    let area = f.area();
    let chunks = Layout::default()
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);

    let step_index = Step::ALL.iter().position(|&s| s == step).unwrap_or(0);
    let step_count = Step::ALL.len();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" [{}] ", step_index + 1),
            Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(step.title(), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" ({}/{}) ", step_index + 1, step_count),
            Style::default().fg(COLOR_GRAY),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(COLOR_GRAY)),
    );
    f.render_widget(header, chunks[0]);

    let body: ratatui::widgets::Paragraph = match step {
        Step::Welcome => {
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "ZeroClaw — fastest, smallest AI assistant.",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from("This wizard configures your agent. Re-entrant: no changes written unless you edit."),
                Line::from(""),
                Line::from(Span::styled("Press Enter to start, q to quit.", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Workspace => {
            let ws = state.workspace_dir.display().to_string();
            let cfg = state.config_path.display().to_string();
            let lines = vec![
                Line::from("Workspace (re-entrant: existing path shown):"),
                Line::from(Span::styled(ws, Style::default().fg(COLOR_GREEN))),
                Line::from("Config:"),
                Line::from(Span::styled(cfg, Style::default().fg(COLOR_GREEN))),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Enter to continue. Edit in config.toml to change path.",
                    Style::default().fg(COLOR_GRAY),
                )),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Provider => {
            match provider_screen {
                ProviderScreen::List => {
                    let items: Vec<ListItem> = providers
                        .iter()
                        .map(|p| {
                            ListItem::new(Line::from(format!(
                                "  {} — {}",
                                p.display_name,
                                if p.local { "local" } else { "API key" }
                            )))
                        })
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Select provider (↑↓ Enter)")
                                .border_style(Style::default().fg(COLOR_GRAY)),
                        )
                        .highlight_style(Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, chunks[1], provider_list_state);
                    let hint = Paragraph::new(Line::from(Span::styled(
                        " ↑↓ select  Enter next  Esc back  q quit",
                        Style::default().fg(COLOR_GRAY),
                    )))
                    .block(Block::default().borders(Borders::TOP));
                    f.render_widget(hint, chunks[2]);
                    return;
                }
                ProviderScreen::ApiKey => draw_provider_api_key_body(state),
                ProviderScreen::OAuth {
                    auth_info,
                    user_input,
                    error,
                    ..
                } => {
                    // Match zeroai config_tui.rs: label (bordered), input (bordered), then info with instructions + URL (no border).
                    let auth_data = auth_info
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().cloned());
                    let hint = auth_data
                        .as_ref()
                        .and_then(|i| i.instructions.clone())
                        .unwrap_or_else(|| "Connecting...".into());
                    let oauth_url = auth_data.as_ref().map(|i| i.url.as_str());

                    let has_info = !hint.is_empty() || oauth_url.is_some();
                    let constraints = if has_info {
                        vec![
                            Constraint::Length(3),
                            Constraint::Length(3),
                            Constraint::Min(3),
                        ]
                    } else {
                        vec![Constraint::Length(3), Constraint::Length(3)]
                    };
                    let oauth_chunks = Layout::default()
                        .direction(ratatui::layout::Direction::Vertical)
                        .constraints(constraints)
                        .split(chunks[1]);

                    let mut idx = 0;
                    let label_para = Paragraph::new(Line::from(Span::styled(
                        "OAuth — complete in browser, then paste code or URL below.",
                        Style::default().fg(COLOR_CYAN),
                    )))
                    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(COLOR_GRAY)));
                    f.render_widget(label_para, oauth_chunks[idx]);
                    idx += 1;

                    let input_title = Line::from(vec![
                        Span::raw(" Input ("),
                        Span::styled("Enter", Style::default().fg(COLOR_YELLOW)),
                        Span::raw(" submit, "),
                        Span::styled("Esc", Style::default().fg(COLOR_YELLOW)),
                        Span::raw(" back) "),
                    ]);
                    let mut input_lines = vec![Line::from(user_input.as_str())];
                    if let Some(ref e) = error {
                        input_lines.push(Line::from(Span::styled(
                            format!("Error: {}", e),
                            Style::default().fg(Color::Red),
                        )));
                    }
                    let input_para = Paragraph::new(input_lines)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(input_title)
                                .border_style(Style::default().fg(COLOR_GRAY)),
                        )
                        .wrap(Wrap { trim: true });
                    f.render_widget(input_para, oauth_chunks[idx]);
                    idx += 1;

                    if has_info {
                        let mut info_content = vec![
                            Line::from(Span::styled(
                                "Instructions: ",
                                Style::default().fg(COLOR_YELLOW),
                            )),
                            Line::from(hint.as_str()),
                        ];
                        if let Some(url) = oauth_url {
                            info_content.push(Line::from(""));
                            info_content.push(Line::from(Span::styled(
                                "Clean URL (copy below):",
                                Style::default().fg(COLOR_CYAN),
                            )));
                            info_content.push(Line::from(url));
                        }
                        let info_para = Paragraph::new(info_content)
                            .wrap(Wrap { trim: false })
                            .block(Block::default().borders(Borders::NONE).title(""));
                        f.render_widget(info_para, oauth_chunks[idx]);
                    }

                    let hint_para = Paragraph::new(Line::from(Span::styled(
                        " Type code/URL  Enter submit  Esc back  q quit",
                        Style::default().fg(COLOR_GRAY),
                    )))
                    .block(Block::default().borders(Borders::TOP));
                    f.render_widget(hint_para, chunks[2]);
                    return;
                }
                ProviderScreen::ModelSelect => {
                    let models = curated_models_for_provider(&state.provider);
                    let items: Vec<ListItem> = models
                        .iter()
                        .map(|(id, display)| ListItem::new(Line::from(format!("  {} — {}", id, display))))
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Select model (↑↓ Enter)")
                                .border_style(Style::default().fg(COLOR_GRAY)),
                        )
                        .highlight_style(Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, chunks[1], model_list_state);
                    let hint = Paragraph::new(Line::from(Span::styled(
                        " ↑↓ select  Enter next  Esc back",
                        Style::default().fg(COLOR_GRAY),
                    )))
                    .block(Block::default().borders(Borders::TOP));
                    f.render_widget(hint, chunks[2]);
                    return;
                }
            }
        }
        Step::Channels => {
            match channels_screen {
                ChannelsScreen::List => {
                    let list_items = channel_list(&state.channels_config);
                    let items: Vec<ListItem> = list_items
                        .iter()
                        .map(|(label, configured)| {
                            let status = if *configured { "✅" } else { "—" };
                            ListItem::new(Line::from(format!(
                                "  {} {} {}",
                                status,
                                label,
                                if *configured { "connected" } else { "" }
                            )))
                        })
                        .chain(std::iter::once(ListItem::new(Line::from(Span::styled(
                            "  Done — continue to next step",
                            Style::default().fg(COLOR_GREEN),
                        )))))
                        .collect();
                    let list = List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Select channel to configure (Enter) or Done")
                                .border_style(Style::default().fg(COLOR_GRAY)),
                        )
                        .highlight_style(Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, chunks[1], channel_list_state);
                    let hint = Paragraph::new(Line::from(Span::styled(
                        " ↑↓ select  Enter edit/done  Esc back  q quit",
                        Style::default().fg(COLOR_GRAY),
                    )))
                    .block(Block::default().borders(Borders::TOP));
                    f.render_widget(hint, chunks[2]);
                    return;
                }
                ChannelsScreen::Input { .. } => draw_channels_body(state, channels_screen),
            }
        }
        Step::ToolMode => {
            let items: Vec<ListItem> = TOOL_MODE_OPTIONS
                .iter()
                .map(|(_, label)| ListItem::new(Line::from(*label)))
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Select tool mode (↑↓ then Enter)")
                        .border_style(Style::default().fg(COLOR_GRAY)),
                )
                .highlight_style(Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD))
                .highlight_symbol("> ");
            f.render_stateful_widget(list, chunks[1], tool_mode_list_state);
            let hint = Paragraph::new(Line::from(Span::styled(
                " ↑↓ select  Enter next  Esc back  q quit",
                Style::default().fg(COLOR_GRAY),
            )))
            .block(Block::default().borders(Borders::TOP));
            f.render_widget(hint, chunks[2]);
            return;
        }
        Step::ProjectContext => {
            let labels = ["User name:", "Timezone:", "Agent name:", "Communication style:"];
            let values = [
                &state.project_ctx.user_name,
                &state.project_ctx.timezone,
                &state.project_ctx.agent_name,
                &state.project_ctx.communication_style,
            ];
            let mut lines = vec![
                Line::from(Span::styled(
                    "Edit fields (Tab/↑↓ next, type to edit). Enter on last field = next step.",
                    Style::default().fg(COLOR_GRAY),
                )),
                Line::from(""),
            ];
            for (i, (label, value)) in labels.iter().zip(values.iter()).enumerate() {
                let display: String = value.chars().take(60).collect();
                let style = if i == project_focused as usize {
                    Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", label), Style::default().fg(COLOR_GRAY)),
                    Span::styled(display, style),
                    Span::styled(
                        if i == project_focused as usize { " █" } else { "" },
                        Style::default().fg(COLOR_CYAN),
                    ),
                ]));
            }
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::ZeroAi => {
            let lines = vec![
                Line::from("ZeroAI provider config (OAuth, multi-provider):"),
                Line::from(""),
                Line::from(Span::styled(
                    "Press r to run `zeroai-proxy config` (TUI for ~/.zeroai).",
                    Style::default().fg(COLOR_YELLOW),
                )),
                Line::from("Then use ZeroAI proxy as your API endpoint in zeroclaw config."),
                Line::from(""),
                Line::from(Span::styled(
                    "r = Run ZeroAI TUI   Enter = continue",
                    Style::default().fg(COLOR_GRAY),
                )),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Summary => draw_summary_body(state),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_GRAY))
        .title(step.title());
    f.render_widget(body.block(block), chunks[1]);

    let hint = Paragraph::new(Line::from(Span::styled(
        " ↑↓ select  Enter next  Esc back  q quit",
        Style::default().fg(COLOR_GRAY),
    )))
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(hint, chunks[2]);
}

fn draw_provider_api_key_body(state: &WizardState) -> ratatui::widgets::Paragraph<'static> {
    let key_display = if state.api_key.is_empty() {
        "not set (optional for some providers)"
    } else {
        "*** set ***"
    };
    let lines = vec![
        Line::from(Span::styled(
            format!("Provider: {}  Model: {}", state.provider, state.model),
            Style::default().fg(COLOR_GREEN),
        )),
        Line::from(""),
        Line::from("API key (type then Enter to pick model):"),
        Line::from(Span::styled(
            if state.api_key.is_empty() {
                key_display.to_string()
            } else {
                format!("{}***", state.api_key.chars().take(4).collect::<String>())
            },
            Style::default().fg(COLOR_YELLOW),
        )),
        Line::from(""),
        Line::from(Span::styled("Esc = back", Style::default().fg(COLOR_GRAY))),
    ];
    Paragraph::new(lines).wrap(Wrap { trim: true })
}

fn draw_channels_body(
    state: &WizardState,
    channels_screen: &ChannelsScreen,
) -> ratatui::widgets::Paragraph<'static> {
    match channels_screen {
        ChannelsScreen::List => {
            let list_items = channel_list(&state.channels_config);
            let mut lines = vec![
                Line::from(Span::styled(
                    "Select a channel to configure (Enter), or select Done to continue.",
                    Style::default().fg(COLOR_GRAY),
                )),
                Line::from(""),
            ];
            for (label, configured) in &list_items {
                let status = if *configured { "✅" } else { "—" };
                lines.push(Line::from(format!(
                    "  {} {} {}",
                    status,
                    label,
                    if *configured { "connected" } else { "" }
                )));
            }
            lines.push(Line::from(Span::styled(
                "  Done — continue to next step",
                Style::default().fg(COLOR_GREEN),
            )));
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        ChannelsScreen::Input {
            channel_index,
            fields,
            focused,
            cursor_pos: _,
            error,
        } => {
            let labels: Vec<&str> = if *channel_index == 0 {
                vec!["Bot token (from @BotFather):", "Allowed users (comma or *):"]
            } else {
                vec![
                    "Bot token:",
                    "Guild ID (optional):",
                    "Allowed user IDs (comma or *):",
                ]
            };
            let mut lines = Vec::new();
            for (i, (label, value)) in labels.iter().zip(fields.iter()).enumerate() {
                let display: String = value.chars().take(60).collect();
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", label), Style::default().fg(COLOR_GRAY)),
                    Span::raw(display),
                    Span::styled(
                        if i == *focused { " █" } else { "" },
                        Style::default().fg(COLOR_CYAN),
                    ),
                ]));
            }
            if let Some(err) = error {
                lines.push(Line::from(Span::styled(
                    format!("Error: {}", err),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter = save   Esc = back",
                Style::default().fg(COLOR_GRAY),
            )));
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
    }
}

fn draw_summary_body(state: &WizardState) -> ratatui::widgets::Paragraph<'static> {
    let dirty_note = if state.dirty.any() {
        " (modified — Enter to save)"
    } else if state.reentrant {
        " (no changes — Enter to exit without writing)"
    } else {
        " (Enter to save)"
    };
    let api_key_status = if state.api_key.is_empty() {
        "not set"
    } else {
        "set"
    };
    let channel_names: Vec<String> = channel_list(&state.channels_config)
        .into_iter()
        .filter(|(_, configured)| *configured)
        .map(|(name, _)| name)
        .collect();
    let channels_line = if channel_names.is_empty() {
        "none".to_string()
    } else {
        channel_names.join(", ")
    };
    let tool_mode = if state.composio_config.enabled {
        "Composio (OAuth)"
    } else {
        "Sovereign (local)"
    };
    let lines = vec![
        Line::from(Span::styled("Summary", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(dirty_note, Style::default().fg(COLOR_YELLOW))),
        Line::from(""),
        Line::from(format!("Workspace:  {}", state.workspace_dir.display())),
        Line::from(format!("Config:     {}", state.config_path.display())),
        Line::from(""),
        Line::from(format!("Provider:   {}", state.provider)),
        Line::from(format!("Model:      {}", state.model)),
        Line::from(format!("API key:    {}", api_key_status)),
        Line::from(""),
        Line::from(format!("Channels:   {}", channels_line)),
        Line::from(format!("Memory:     {} (auto_save: {})", state.memory_config.backend, state.memory_config.auto_save)),
        Line::from(format!("Tool mode:  {}", tool_mode)),
        Line::from(format!("Secrets:    {}", if state.secrets_config.encrypt { "encrypted" } else { "plain" })),
        Line::from(""),
        Line::from(format!("Agent:      {}", state.project_ctx.agent_name)),
        Line::from(format!("User:       {}", state.project_ctx.user_name)),
        Line::from(format!("Timezone:   {}", state.project_ctx.timezone)),
        Line::from(""),
        Line::from(Span::styled(
            "Enter = Save and exit   Esc = Back",
            Style::default().fg(COLOR_GRAY),
        )),
    ];
    Paragraph::new(lines).wrap(Wrap { trim: true })
}
