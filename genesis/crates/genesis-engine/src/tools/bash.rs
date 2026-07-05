//! The bash tool — runs a shell command in the workspace root.
//!
//! This is the one surface whose contract is "interpret shell syntax", so it
//! uses [`crate::shell::shell_command`] (shell-string mode) with the model's
//! command passed through verbatim as the whole script.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{EngineError, Result};
use crate::shell::shell_command;
use crate::types::ToolDef;

use super::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct BashTool {
    root: PathBuf,
}

impl BashTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_string(),
            description: "Run a shell command in the workspace root and return its output. Commands time out (default 120s).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run" },
                    "timeout_secs": { "type": "integer", "description": "Optional timeout in seconds (max 600)" },
                },
                "required": ["command"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let script = super::required_str(input, "command", "bash")?;
        let timeout = Duration::from_secs(
            input["timeout_secs"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS),
        );
        let mut command = shell_command(script);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| EngineError::Tool {
            name: "bash".to_string(),
            message: format!("spawn failed: {e}"),
        })?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| EngineError::Tool {
                name: "bash".to_string(),
                message: format!("command timed out after {}s", timeout.as_secs()),
            })??;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr]\n");
            result.push_str(&stderr);
        }
        if !output.status.success() {
            return Err(EngineError::Tool {
                name: "bash".to_string(),
                message: format!(
                    "exit status {}\n{result}",
                    output
                        .status
                        .code()
                        .map_or("signal".to_string(), |c| c.to_string())
                ),
            });
        }
        if result.is_empty() {
            result.push_str("(no output)");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> (tempfile::TempDir, BashTool) {
        let dir = tempfile::tempdir().unwrap();
        let tool = BashTool::new(dir.path().to_path_buf());
        (dir, tool)
    }

    #[tokio::test]
    async fn runs_in_workspace_root_and_captures_stdout() {
        let (dir, tool) = tool();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let out = tool
            .run(&json!({ "command": if cfg!(windows) { "dir /b" } else { "ls" } }))
            .await
            .unwrap();
        assert!(out.contains("marker.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_a_tool_error_with_output() {
        let (_dir, tool) = tool();
        let err = tool
            .run(&json!({ "command": "echo oops >&2; exit 3" }))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exit status 3"), "got: {text}");
        assert!(text.contains("oops"), "got: {text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_command() {
        let (_dir, tool) = tool();
        let err = tool
            .run(&json!({ "command": "sleep 30", "timeout_secs": 1 }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }
}
