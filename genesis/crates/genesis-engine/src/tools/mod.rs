//! The built-in tool set and the [`Tool`] trait.

mod bash;
mod fs;
mod search;

pub use bash::BashTool;
pub use fs::{EditFileTool, ReadFileTool, WriteFileTool};
pub use search::{GlobTool, GrepTool};

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{EngineError, Result};
use crate::types::ToolDef;

/// Truncation cap applied to every tool's output before it reaches the model.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One agent tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's definition as advertised to the model.
    fn def(&self) -> ToolDef;

    /// Run the tool. `Err` becomes an `is_error` tool result, not an engine
    /// failure — the model gets to see and react to tool errors.
    async fn run(&self, input: &Value) -> Result<String>;
}

/// The set of tools available to one agent.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// The default tool set, confined to `root`.
    pub fn builtin(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut registry = Self::default();
        registry.register(Box::new(ReadFileTool::new(root.clone())));
        registry.register(Box::new(WriteFileTool::new(root.clone())));
        registry.register(Box::new(EditFileTool::new(root.clone())));
        registry.register(Box::new(GlobTool::new(root.clone())));
        registry.register(Box::new(GrepTool::new(root.clone())));
        registry.register(Box::new(BashTool::new(root)));
        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.def().name, tool);
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.def()).collect()
    }

    pub async fn run(&self, name: &str, input: &Value) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| EngineError::UnknownTool(name.to_string()))?;
        tool.run(input).await.map(truncate_output)
    }
}

/// Cap tool output so a single result cannot flood the context window.
fn truncate_output(mut output: String) -> String {
    if output.len() > MAX_OUTPUT_BYTES {
        let mut cut = MAX_OUTPUT_BYTES;
        while !output.is_char_boundary(cut) {
            cut -= 1;
        }
        output.truncate(cut);
        output.push_str("\n[output truncated]");
    }
    output
}

/// Resolve `path` against `root`, rejecting escapes.
///
/// The check is lexical (no filesystem access), so it also works for paths
/// that do not exist yet (write targets).
pub(crate) fn confine(root: &Path, path: &str) -> Result<PathBuf> {
    let joined = {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return escape_error(path);
                }
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    if normalized.starts_with(root) {
        Ok(normalized)
    } else {
        escape_error(path)
    }
}

fn escape_error(path: &str) -> Result<PathBuf> {
    Err(EngineError::Tool {
        name: "path".to_string(),
        message: format!("'{path}' escapes the workspace root"),
    })
}

/// Fetch a required string field from tool input.
pub(crate) fn required_str<'a>(input: &'a Value, field: &str, tool: &str) -> Result<&'a str> {
    input[field].as_str().ok_or_else(|| EngineError::Tool {
        name: tool.to_string(),
        message: format!("missing required string field '{field}'"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_allows_relative_paths_inside_root() {
        let root = Path::new("/work");
        assert_eq!(
            confine(root, "src/main.rs").unwrap(),
            PathBuf::from("/work/src/main.rs")
        );
        assert_eq!(
            confine(root, "a/./b/../c.txt").unwrap(),
            PathBuf::from("/work/a/c.txt")
        );
    }

    #[test]
    fn confine_rejects_escapes() {
        let root = Path::new("/work");
        assert!(confine(root, "../etc/passwd").is_err());
        assert!(confine(root, "a/../../etc/passwd").is_err());
        assert!(confine(root, "/etc/passwd").is_err());
    }

    #[test]
    fn confine_accepts_absolute_paths_inside_root() {
        let root = Path::new("/work");
        assert_eq!(
            confine(root, "/work/a.txt").unwrap(),
            PathBuf::from("/work/a.txt")
        );
    }

    #[test]
    fn truncate_output_caps_large_output_at_char_boundary() {
        let big = "é".repeat(MAX_OUTPUT_BYTES); // 2 bytes per char
        let out = truncate_output(big);
        assert!(out.len() <= MAX_OUTPUT_BYTES + "\n[output truncated]".len());
        assert!(out.ends_with("[output truncated]"));
    }
}
