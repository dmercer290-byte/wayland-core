//! Search tools: glob and grep.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::GlobBuilder;
use regex::Regex;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::error::{EngineError, Result};
use crate::types::ToolDef;

use super::Tool;

/// Cap on reported matches so pathological patterns stay bounded.
const MAX_MATCHES: usize = 500;

/// Directories never worth descending into.
fn skip_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules")
}

fn walk(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir() && e.file_name().to_str().map(skip_dir).unwrap_or(false))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
}

pub struct GlobTool {
    root: PathBuf,
}

impl GlobTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "glob".to_string(),
            description: "Find files matching a glob pattern (e.g. **/*.rs). Paths are reported relative to the workspace root.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern matched against workspace-relative paths" },
                },
                "required": ["pattern"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let pattern = super::required_str(input, "pattern", "glob")?;
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| EngineError::Tool {
                name: "glob".to_string(),
                message: format!("bad pattern: {e}"),
            })?
            .compile_matcher();
        let root = self.root.clone();
        let hits = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            for entry in walk(&root) {
                let Ok(rel) = entry.path().strip_prefix(&root) else {
                    continue;
                };
                if matcher.is_match(rel) {
                    hits.push(rel.display().to_string());
                    if hits.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            hits
        })
        .await
        .map_err(|e| EngineError::Tool {
            name: "glob".to_string(),
            message: e.to_string(),
        })?;
        if hits.is_empty() {
            Ok("no files matched".to_string())
        } else {
            Ok(hits.join("\n"))
        }
    }
}

pub struct GrepTool {
    root: PathBuf,
}

impl GrepTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "grep".to_string(),
            description:
                "Search file contents with a regular expression. Returns 'path:line: text' matches."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Rust-flavored regular expression" },
                },
                "required": ["pattern"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let pattern = super::required_str(input, "pattern", "grep")?;
        let regex = Regex::new(pattern).map_err(|e| EngineError::Tool {
            name: "grep".to_string(),
            message: format!("bad pattern: {e}"),
        })?;
        let root = self.root.clone();
        let hits = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            'files: for entry in walk(&root) {
                // Binary and non-UTF-8 files are silently skipped.
                let Ok(content) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .display()
                    .to_string();
                for (idx, line) in content.lines().enumerate() {
                    if regex.is_match(line) {
                        hits.push(format!("{rel}:{}: {line}", idx + 1));
                        if hits.len() >= MAX_MATCHES {
                            break 'files;
                        }
                    }
                }
            }
            hits
        })
        .await
        .map_err(|e| EngineError::Tool {
            name: "grep".to_string(),
            message: e.to_string(),
        })?;
        if hits.is_empty() {
            Ok("no matches".to_string())
        } else {
            Ok(hits.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "find the needle here\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "needle\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn glob_matches_relative_paths_and_skips_git() {
        let dir = fixture();
        let tool = GlobTool::new(dir.path().to_path_buf());
        let out = tool.run(&json!({ "pattern": "**/*.rs" })).await.unwrap();
        assert_eq!(out, "src/main.rs");
        let all = tool.run(&json!({ "pattern": "**/*" })).await.unwrap();
        assert!(!all.contains(".git"));
    }

    #[tokio::test]
    async fn grep_reports_line_numbers_and_skips_git() {
        let dir = fixture();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = tool.run(&json!({ "pattern": "needle" })).await.unwrap();
        assert_eq!(out, "notes.md:1: find the needle here");
    }

    #[tokio::test]
    async fn grep_rejects_invalid_regex() {
        let dir = fixture();
        let tool = GrepTool::new(dir.path().to_path_buf());
        assert!(tool.run(&json!({ "pattern": "([" })).await.is_err());
    }
}
