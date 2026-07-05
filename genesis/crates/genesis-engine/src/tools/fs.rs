//! File tools: read_file, write_file, edit_file.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;

use crate::error::{EngineError, Result};
use crate::types::ToolDef;

use super::{confine, required_str, Tool};

pub struct ReadFileTool {
    root: PathBuf,
}

impl ReadFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file. Returns the full contents.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root" },
                },
                "required": ["path"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let path = confine(&self.root, required_str(input, "path", "read_file")?)?;
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| EngineError::Tool {
                name: "read_file".to_string(),
                message: format!("{}: {e}", path.display()),
            })
    }
}

pub struct WriteFileTool {
    root: PathBuf,
}

impl WriteFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".to_string(),
            description: "Write content to a file, creating it (and parent directories) if needed, overwriting if it exists.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root" },
                    "content": { "type": "string", "description": "Full file content to write" },
                },
                "required": ["path", "content"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let path = confine(&self.root, required_str(input, "path", "write_file")?)?;
        let content = required_str(input, "content", "write_file")?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| EngineError::Tool {
                name: "write_file".to_string(),
                message: format!("{}: {e}", path.display()),
            })?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

pub struct EditFileTool {
    root: PathBuf,
}

impl EditFileTool {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".to_string(),
            description: "Replace an exact string in a file. old_string must appear exactly once; include enough surrounding context to make it unique.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to the workspace root" },
                    "old_string": { "type": "string", "description": "Exact text to replace (must be unique in the file)" },
                    "new_string": { "type": "string", "description": "Replacement text" },
                },
                "required": ["path", "old_string", "new_string"],
            }),
        }
    }

    async fn run(&self, input: &Value) -> Result<String> {
        let path = confine(&self.root, required_str(input, "path", "edit_file")?)?;
        let old = required_str(input, "old_string", "edit_file")?;
        let new = required_str(input, "new_string", "edit_file")?;
        if old.is_empty() {
            return Err(EngineError::Tool {
                name: "edit_file".to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| EngineError::Tool {
                name: "edit_file".to_string(),
                message: format!("{}: {e}", path.display()),
            })?;
        let matches = content.matches(old).count();
        if matches == 0 {
            return Err(EngineError::Tool {
                name: "edit_file".to_string(),
                message: "old_string not found in file".to_string(),
            });
        }
        if matches > 1 {
            return Err(EngineError::Tool {
                name: "edit_file".to_string(),
                message: format!(
                    "old_string matches {matches} times; add context to make it unique"
                ),
            });
        }
        let updated = content.replacen(old, new, 1);
        tokio::fs::write(&path, updated).await?;
        Ok(format!("edited {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let dir = root();
        let write = WriteFileTool::new(dir.path().to_path_buf());
        let read = ReadFileTool::new(dir.path().to_path_buf());
        write
            .run(&json!({ "path": "sub/a.txt", "content": "hello" }))
            .await
            .unwrap();
        let content = read.run(&json!({ "path": "sub/a.txt" })).await.unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn read_outside_root_is_rejected() {
        let dir = root();
        let read = ReadFileTool::new(dir.path().to_path_buf());
        let err = read
            .run(&json!({ "path": "../../etc/passwd" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("escapes the workspace root"));
    }

    #[tokio::test]
    async fn edit_replaces_unique_match_only() {
        let dir = root();
        std::fs::write(dir.path().join("f.txt"), "aaa unique bbb").unwrap();
        let edit = EditFileTool::new(dir.path().to_path_buf());
        edit.run(&json!({
            "path": "f.txt", "old_string": "unique", "new_string": "replaced",
        }))
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "aaa replaced bbb"
        );
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_and_missing_matches() {
        let dir = root();
        std::fs::write(dir.path().join("f.txt"), "dup dup").unwrap();
        let edit = EditFileTool::new(dir.path().to_path_buf());
        let ambiguous = edit
            .run(&json!({ "path": "f.txt", "old_string": "dup", "new_string": "x" }))
            .await
            .unwrap_err();
        assert!(ambiguous.to_string().contains("2 times"));
        let missing = edit
            .run(&json!({ "path": "f.txt", "old_string": "absent", "new_string": "x" }))
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("not found"));
    }
}
