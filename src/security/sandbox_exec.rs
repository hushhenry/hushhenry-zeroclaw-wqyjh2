//! macOS `sandbox-exec` sandbox backend.

use crate::security::traits::Sandbox;
use std::path::Path;
use std::process::Command;

/// `sandbox-exec` backend for macOS.
#[derive(Debug, Clone, Default)]
pub struct SandboxExecSandbox;

impl SandboxExecSandbox {
    /// Create a new sandbox backend instance.
    pub fn new() -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "sandbox-exec not found",
            ))
        }
    }

    /// Probe if `sandbox-exec` is available for auto-detection.
    pub fn probe() -> std::io::Result<Self> {
        Self::new()
    }

    fn is_installed() -> bool {
        Command::new("sandbox-exec").arg("-h").output().is_ok()
    }

    fn escape_profile_string(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn build_profile(workspace: Option<&Path>) -> String {
        let mut lines = vec![
            "(version 1)".to_string(),
            "(deny default)".to_string(),
            "(import \"system.sb\")".to_string(),
            "(allow process-exec)".to_string(),
            "(allow process-fork)".to_string(),
            "(allow file-read*)".to_string(),
            "(allow file-write* (subpath \"/tmp\"))".to_string(),
            "(allow file-write* (subpath \"/private/tmp\"))".to_string(),
        ];

        if let Some(workspace) = workspace {
            let workspace = Self::escape_profile_string(&workspace.to_string_lossy());
            lines.push(format!("(allow file-write* (subpath \"{workspace}\"))"));
        }

        lines.join("\n")
    }
}

impl Sandbox for SandboxExecSandbox {
    fn wrap_command(&self, cmd: &mut Command) -> std::io::Result<()> {
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        let profile = Self::build_profile(cmd.get_current_dir());

        let mut sandbox_cmd = Command::new("sandbox-exec");
        sandbox_cmd.arg("-p");
        sandbox_cmd.arg(profile);
        sandbox_cmd.arg(program);
        sandbox_cmd.args(args);

        *cmd = sandbox_cmd;
        Ok(())
    }

    fn is_available(&self) -> bool {
        Self::is_installed()
    }

    fn name(&self) -> &str {
        "sandbox-exec"
    }

    fn description(&self) -> &str {
        "macOS application sandbox (requires sandbox-exec)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_exec_name_is_stable() {
        assert_eq!(SandboxExecSandbox.name(), "sandbox-exec");
    }

    #[test]
    fn sandbox_exec_wrap_command_prefixes_program() {
        let sandbox = SandboxExecSandbox;
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        cmd.current_dir("/tmp");

        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert_eq!(cmd.get_program().to_string_lossy(), "sandbox-exec");
        assert!(args.len() >= 4);
        assert_eq!(args[0], "-p");
        assert!(args[1].contains("(version 1)"));
        assert_eq!(args[2], "echo");
        assert_eq!(args[3], "hello");
    }
}
