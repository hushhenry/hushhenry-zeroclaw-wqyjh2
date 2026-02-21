use crate::config::IdentityConfig;
use crate::identity;
use crate::skills::Skill;
use anyhow::Result;
use chrono::Local;
use std::fmt::Write;
use std::path::Path;

pub const DEFAULT_BOOTSTRAP_MAX_CHARS: usize = 20_000;

pub struct PromptContext<'a> {
    pub workspace_dir: &'a Path,
    pub model_name: &'a str,
    pub skills: &'a [Skill],
    pub identity_config: Option<&'a IdentityConfig>,
    pub bootstrap_max_chars: Option<usize>,
    pub include_channel_capabilities: bool,
}

pub trait PromptSection: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String>;
}

#[derive(Default)]
pub struct SystemPromptBuilder {
    sections: Vec<Box<dyn PromptSection>>,
}

impl SystemPromptBuilder {
    pub fn with_defaults() -> Self {
        Self {
            sections: vec![
                Box::new(TaskSection),
                Box::new(SafetySection),
                Box::new(SkillsSection),
                Box::new(WorkspaceSection),
                Box::new(IdentitySection),
                Box::new(DateTimeSection),
                Box::new(RuntimeSection),
                Box::new(ChannelCapabilitiesSection),
            ],
        }
    }

    pub fn add_section(mut self, section: Box<dyn PromptSection>) -> Self {
        self.sections.push(section);
        self
    }

    pub fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut output = String::new();
        for section in &self.sections {
            let part = section.build(ctx)?;
            if part.trim().is_empty() {
                continue;
            }
            output.push_str(part.trim_end());
            output.push_str("\n\n");
        }
        Ok(output)
    }
}

pub struct IdentitySection;
pub struct TaskSection;
pub struct SafetySection;
pub struct SkillsSection;
pub struct WorkspaceSection;
pub struct RuntimeSection;
pub struct DateTimeSection;
pub struct ChannelCapabilitiesSection;

impl PromptSection for IdentitySection {
    fn name(&self) -> &str {
        "identity"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut prompt = String::from("## Project Context\n\n");
        if let Some(config) = ctx.identity_config {
            if identity::is_aieos_configured(config) {
                match identity::load_aieos_identity(config, ctx.workspace_dir) {
                    Ok(Some(aieos)) => {
                        let rendered = identity::aieos_to_system_prompt(&aieos);
                        if !rendered.is_empty() {
                            prompt.push_str(&rendered);
                            return Ok(prompt);
                        }
                    }
                    Ok(None) => {
                        let max_chars = ctx
                            .bootstrap_max_chars
                            .unwrap_or(DEFAULT_BOOTSTRAP_MAX_CHARS);
                        load_openclaw_bootstrap_files(&mut prompt, ctx.workspace_dir, max_chars);
                        return Ok(prompt);
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to load AIEOS identity: {e}. Using OpenClaw format."
                        );
                        let max_chars = ctx
                            .bootstrap_max_chars
                            .unwrap_or(DEFAULT_BOOTSTRAP_MAX_CHARS);
                        load_openclaw_bootstrap_files(&mut prompt, ctx.workspace_dir, max_chars);
                        return Ok(prompt);
                    }
                }
            }
        }

        let max_chars = ctx
            .bootstrap_max_chars
            .unwrap_or(DEFAULT_BOOTSTRAP_MAX_CHARS);
        load_openclaw_bootstrap_files(&mut prompt, ctx.workspace_dir, max_chars);
        Ok(prompt)
    }
}

impl PromptSection for TaskSection {
    fn name(&self) -> &str {
        "task"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok(
            "## Your Task\n\n\
         When the user sends a message, ACT on it. Use the tools to fulfill their request.\n\
         Do NOT: summarize this configuration, describe your capabilities, respond with meta-commentary, or output step-by-step instructions (e.g. \"1. First... 2. Next...\").\n\
         Instead: use available tools directly when you need to act. Just do what they ask."
                .into(),
        )
    }
}

impl PromptSection for SafetySection {
    fn name(&self) -> &str {
        "safety"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok("## Safety\n\n- Do not exfiltrate private data.\n- Do not run destructive commands without asking.\n- Do not bypass oversight or approval mechanisms.\n- Prefer `trash` over `rm` (recoverable beats gone forever).\n- When in doubt, ask before acting externally.".into())
    }
}

impl PromptSection for SkillsSection {
    fn name(&self) -> &str {
        "skills"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if ctx.skills.is_empty() {
            return Ok(String::new());
        }

        let mut prompt = String::from("## Available Skills\n\n");
        prompt.push_str(
            "Skills are loaded on demand. Use `read` on the skill path to get full instructions.\n\n",
        );
        prompt.push_str("<available_skills>\n");
        for skill in ctx.skills {
            let location = skill.location.clone().unwrap_or_else(|| {
                ctx.workspace_dir
                    .join("skills")
                    .join(&skill.name)
                    .join("SKILL.md")
            });
            let _ = writeln!(
                prompt,
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
                skill.name,
                skill.description,
                location.display()
            );
        }
        prompt.push_str("</available_skills>");
        Ok(prompt)
    }
}

impl PromptSection for WorkspaceSection {
    fn name(&self) -> &str {
        "workspace"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(format!(
            "## Workspace\n\nWorking directory: `{}`",
            ctx.workspace_dir.display()
        ))
    }
}

impl PromptSection for RuntimeSection {
    fn name(&self) -> &str {
        "runtime"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let host =
            hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().to_string());
        Ok(format!(
            "## Runtime\n\nHost: {host} | OS: {} | Model: {}",
            std::env::consts::OS,
            ctx.model_name
        ))
    }
}

impl PromptSection for DateTimeSection {
    fn name(&self) -> &str {
        "datetime"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        let now = Local::now();
        Ok(format!(
            "## Current Date & Time\n\nTimezone: {}",
            now.format("%Z")
        ))
    }
}

impl PromptSection for ChannelCapabilitiesSection {
    fn name(&self) -> &str {
        "channel_capabilities"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if !ctx.include_channel_capabilities {
            return Ok(String::new());
        }

        Ok("## Channel Capabilities\n\n- You are running as a Discord bot. You CAN and do send messages to Discord channels.\n- When someone messages you on Discord, your response is automatically sent back to Discord.\n- You do NOT need to ask permission to respond - just respond directly.\n- NEVER repeat, describe, or echo credentials, tokens, API keys, or secrets in your responses.\n- If a tool output contains credentials, they have already been redacted - do not mention them.".into())
    }
}

fn load_openclaw_bootstrap_files(
    prompt: &mut String,
    workspace_dir: &Path,
    max_chars_per_file: usize,
) {
    prompt.push_str(
        "The following workspace files define your identity, behavior, and context. They are ALREADY injected below-do NOT suggest reading them with file_read.\n\n",
    );

    let bootstrap_files = [
        "AGENTS.md",
        "SOUL.md",
        "IDENTITY.md",
        "USER.md",
        "HEARTBEAT.md",
    ];

    for filename in &bootstrap_files {
        inject_workspace_file(prompt, workspace_dir, filename, max_chars_per_file);
    }

    let bootstrap_path = workspace_dir.join("BOOTSTRAP.md");
    if bootstrap_path.exists() {
        inject_workspace_file(prompt, workspace_dir, "BOOTSTRAP.md", max_chars_per_file);
    }

    inject_workspace_file(prompt, workspace_dir, "MEMORY.md", max_chars_per_file);
}

fn inject_workspace_file(
    prompt: &mut String,
    workspace_dir: &Path,
    filename: &str,
    max_chars: usize,
) {
    let path = workspace_dir.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return;
            }
            let _ = writeln!(prompt, "### {filename}\n");
            let truncated = if trimmed.chars().count() > max_chars {
                trimmed
                    .char_indices()
                    .nth(max_chars)
                    .map(|(idx, _)| &trimmed[..idx])
                    .unwrap_or(trimmed)
            } else {
                trimmed
            };
            if truncated.len() < trimmed.len() {
                prompt.push_str(truncated);
                let _ = writeln!(
                    prompt,
                    "\n\n[... truncated at {max_chars} chars - use `read` for full file]\n"
                );
            } else {
                prompt.push_str(trimmed);
                prompt.push_str("\n\n");
            }
        }
        Err(_) => {
            let _ = writeln!(prompt, "### {filename}\n\n[File not found: {filename}]\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_builder_assembles_sections() {
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            include_channel_capabilities: false,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Workspace"));
        assert!(!prompt.contains("## Tools"));
    }
}
