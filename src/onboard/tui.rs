//! Ratatui-based interactive onboard wizard.
//!
//! Re-entrant: loads existing config when present; only writes config and
//! persists workspace when the user actually changes something (dirty).

use crate::config::{
    ChannelsConfig, ComposioConfig, Config, MemoryConfig, SecretsConfig,
};
use crate::memory::default_memory_backend_key;
use crate::onboard::channels::{self, channel_list};
use crate::onboard::wizard::{
    memory_config_defaults_for_backend, persist_workspace_selection,
    scaffold_main_workspace, ProjectContext,
};
use anyhow::{Context, Result};
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
    widgets::{Block, Borders, ListState, Paragraph, Wrap},
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

/// Wizard step index (0-based for screens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Welcome = 0,
    Workspace,
    Provider,
    Channels,
    ToolMode,
    Memory,
    ProjectContext,
    ZeroAi,
    Summary,
}

impl Step {
    const ALL: &'static [Step] = &[
        Step::Welcome,
        Step::Workspace,
        Step::Provider,
        Step::Channels,
        Step::ToolMode,
        Step::Memory,
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
            Step::Memory => "Memory",
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
        let memory_backend = default_memory_backend_key();
        Self {
            workspace_dir,
            config_path,
            provider: "openrouter".to_string(),
            api_key: String::new(),
            api_url: None,
            model: "anthropic/claude-sonnet-4.5".to_string(),
            channels_config: ChannelsConfig::default(),
            composio_config: ComposioConfig::default(),
            secrets_config: SecretsConfig::default(),
            memory_config: memory_config_defaults_for_backend(memory_backend),
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
                                        .unwrap_or_else(|| (String::new(), String::new(), String::new()));
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
                    } => {
                        match key.code {
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
                        }
                    }
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
        .constraints([Constraint::Length(3), Constraint::Min(8), Constraint::Length(2)])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Channels Repair ",
            Style::default().fg(COLOR_CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("— update tokens and allowlists", Style::default().fg(COLOR_GRAY)),
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
                .map(|(i, (label, configured))| {
                    let status = if *configured { "✅" } else { "—" };
                    Line::from(format!("{}  {} {}", status, label, if *configured { "connected" } else { "" }))
                })
                .chain(std::iter::once(Line::from(
                    Span::styled("Done — save and exit", Style::default().fg(COLOR_GREEN)),
                )))
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
            cursor_pos,
            error,
        } => {
            let labels: Vec<&str> = if *channel_index == 0 {
                vec!["Bot token (from @BotFather):", "Allowed users (comma or *):"]
            } else {
                vec!["Bot token:", "Guild ID (optional):", "Allowed user IDs (comma or *):"]
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

fn run_tui_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, state: &mut WizardState) -> Result<Option<Config>> {
    let mut step = Step::Welcome;
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    loop {
        terminal.draw(|f| draw(f, state, step, &list_state))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }

                match step {
                    Step::Welcome => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                            KeyCode::Enter | KeyCode::Down => {
                                step = Step::Workspace;
                            }
                            _ => {}
                        }
                    }
                    Step::Workspace
                    | Step::Provider
                    | Step::Channels
                    | Step::ToolMode
                    | Step::Memory
                    | Step::ProjectContext => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                if let Some(prev) = step.prev() {
                                    step = prev;
                                } else {
                                    return Ok(None);
                                }
                            }
                            KeyCode::Enter | KeyCode::Char('n') => {
                                if let Some(next) = step.next() {
                                    step = next;
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(prev) = step.prev() {
                                    step = prev;
                                }
                            }
                            _ => {}
                        }
                    }
                    Step::ZeroAi => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                if let Some(prev) = step.prev() {
                                    step = prev;
                                } else {
                                    return Ok(None);
                                }
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
                                if let Some(next) = step.next() {
                                    step = next;
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(prev) = step.prev() {
                                    step = prev;
                                }
                            }
                            _ => {}
                        }
                    }
                    Step::Summary => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => {
                                step = Step::ZeroAi;
                            }
                            KeyCode::Enter => {
                                let should_save =
                                    state.dirty.any() || !state.reentrant;
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
                        }
                    }
                }
            }
        }
    }
}

/// Run ZeroAI TUI config (zeroai-proxy config). Blocks until user exits.
fn run_zeroai_proxy_config() {
    let status = Command::new("zeroai-proxy")
        .arg("config")
        .status();
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
        Span::styled(
            step.title(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
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

    let body = match step {
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
                Line::from(Span::styled("Press Enter to continue. Edit in config.toml to change path.", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Provider => {
            let key_display = if state.api_key.is_empty() {
                "not set"
            } else {
                "*** set ***"
            };
            let lines = vec![
                Line::from("Provider & API Key:"),
                Line::from(Span::styled(
                    format!("Provider: {}  Model: {}", state.provider, state.model),
                    Style::default().fg(COLOR_GREEN),
                )),
                Line::from(Span::styled(
                    format!("API key: {}", key_display),
                    Style::default().fg(COLOR_YELLOW),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Enter to continue. Set api_key and default_provider in config.toml.",
                    Style::default().fg(COLOR_GRAY),
                )),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Channels => {
            let has = state.channels_config.telegram.is_some()
                || state.channels_config.discord.is_some()
                || state.channels_config.slack.is_some();
            let ch = if has {
                "configured"
            } else {
                "none"
            };
            let lines = vec![
                Line::from("Channels:"),
                Line::from(Span::styled(ch, Style::default().fg(COLOR_GREEN))),
                Line::from(""),
                Line::from(Span::styled(
                    "Press Enter to continue. Use zeroclaw onboard --channels-only for channel setup.",
                    Style::default().fg(COLOR_GRAY),
                )),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::ToolMode => {
            let mode = if state.composio_config.enabled {
                "Composio (OAuth)"
            } else {
                "Sovereign (local)"
            };
            let lines = vec![
                Line::from("Tool mode:"),
                Line::from(Span::styled(mode, Style::default().fg(COLOR_GREEN))),
                Line::from(""),
                Line::from(Span::styled("Press Enter to continue.", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Memory => {
            let lines = vec![
                Line::from("Memory:"),
                Line::from(Span::styled(
                    format!("Backend: {} (auto_save: {})", state.memory_config.backend, state.memory_config.auto_save),
                    Style::default().fg(COLOR_GREEN),
                )),
                Line::from(""),
                Line::from(Span::styled("Press Enter to continue.", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::ProjectContext => {
            let lines = vec![
                Line::from("Project context:"),
                Line::from(Span::styled(
                    format!("Agent: {}  User: {}  TZ: {}", state.project_ctx.agent_name, state.project_ctx.user_name, state.project_ctx.timezone),
                    Style::default().fg(COLOR_GREEN),
                )),
                Line::from(""),
                Line::from(Span::styled("Press Enter to continue.", Style::default().fg(COLOR_GRAY))),
            ];
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
                Line::from(Span::styled("r = Run ZeroAI TUI   Enter = continue", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
        Step::Summary => {
            let dirty_note = if state.dirty.any() {
                " (modified — Enter to save)"
            } else if state.reentrant {
                " (no changes — Enter to exit without writing)"
            } else {
                " (Enter to save)"
            };
            let lines = vec![
                Line::from("Summary"),
                Line::from(Span::styled(dirty_note, Style::default().fg(COLOR_YELLOW))),
                Line::from(""),
                Line::from(format!("Workspace: {}", state.workspace_dir.display())),
                Line::from(format!("Provider: {}  Model: {}", state.provider, state.model)),
                Line::from(""),
                Line::from(Span::styled("Enter = Save and exit   Esc = Back", Style::default().fg(COLOR_GRAY))),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: true })
        }
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
