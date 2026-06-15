// Lane C3: the install lockfile.
//
// `installed.lock.json` records every committed install with its commit-pinned
// sha so an install is reproducible and auditable. `installed_at` is supplied
// by the caller — lib code never reads the wall clock, which keeps records
// deterministic under test and replay.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin::error::Result;

/// One committed install, keyed by (plugin, marketplace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub plugin: String,
    pub marketplace: String,
    /// Human-readable origin descriptor (e.g. `github:owner/repo`, `path:./x`).
    pub source: String,
    /// The exact commit pinned at resolve time, when the source was cloned.
    pub resolved_sha: Option<String>,
    pub version: String,
    pub grade: String,
    pub installed_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    installed: Vec<InstallRecord>,
}

fn lock_path(plugins_root: &Path) -> PathBuf {
    plugins_root.join("installed.lock.json")
}

fn load(plugins_root: &Path) -> Result<LockFile> {
    let p = lock_path(plugins_root);
    if !p.exists() {
        return Ok(LockFile::default());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(&p)?)?)
}

fn store(plugins_root: &Path, f: &LockFile) -> Result<()> {
    std::fs::create_dir_all(plugins_root)?;
    let bytes = serde_json::to_vec_pretty(f)?;
    wcore_config::atomic_write(lock_path(plugins_root), &bytes)?;
    Ok(())
}

/// Upsert a record (replacing any prior record for the same plugin+marketplace).
pub fn record_install(plugins_root: &Path, rec: InstallRecord) -> Result<()> {
    let mut f = load(plugins_root)?;
    f.installed
        .retain(|r| !(r.plugin == rec.plugin && r.marketplace == rec.marketplace));
    f.installed.push(rec);
    store(plugins_root, &f)
}

/// Read all install records.
pub fn read_lock(plugins_root: &Path) -> Result<Vec<InstallRecord>> {
    Ok(load(plugins_root)?.installed)
}

/// Remove the record for a plugin+marketplace. Returns whether it existed.
pub fn remove_record(plugins_root: &Path, plugin: &str, marketplace: &str) -> Result<bool> {
    let mut f = load(plugins_root)?;
    let before = f.installed.len();
    f.installed
        .retain(|r| !(r.plugin == plugin && r.marketplace == marketplace));
    let removed = f.installed.len() != before;
    if removed {
        store(plugins_root, &f)?;
    }
    Ok(removed)
}
