//! The format-adapter seam. An adapter parses a foreign plugin laid out on
//! disk and lowers it into a [`CanonicalDraft`]. Only adapters know foreign
//! formats; everything downstream is format-blind.

use std::path::Path;

use crate::Result;
use crate::model::{CanonicalDraft, SourceEntry};

pub trait PluginFormatAdapter: Send + Sync {
    /// Stable adapter id, e.g. `"claude-code"`, `"mcp-registry"`.
    fn id(&self) -> &'static str;
    /// Sniff whether this adapter recognizes the layout rooted at `root`.
    fn detect(&self, root: &Path) -> bool;
    /// Lower a plugin (already fetched into the quarantine/cache at `root`),
    /// listed under `marketplace` as `entry`, into a Genesis-native draft.
    fn lower(&self, marketplace: &str, entry: &SourceEntry, root: &Path) -> Result<CanonicalDraft>;
}

/// First format whose marker matches, by priority. Returns the adapter id.
pub fn detect_format(root: &Path) -> Option<String> {
    if root.join(".claude-plugin/plugin.json").exists()
        || (root.join("skills").is_dir() && root.join(".mcp.json").exists())
    {
        return Some("claude-code".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detects_claude_code_by_marker_dir() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude-plugin")).unwrap();
        fs::write(
            d.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"x"}"#,
        )
        .unwrap();
        assert_eq!(detect_format(d.path()).as_deref(), Some("claude-code"));
    }

    #[test]
    fn unknown_when_no_markers() {
        let d = tempdir().unwrap();
        assert!(detect_format(d.path()).is_none());
    }
}
