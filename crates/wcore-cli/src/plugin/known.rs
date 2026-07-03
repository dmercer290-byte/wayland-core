// Lane C3: the registry of known marketplaces.
//
// Persisted as `known_marketplaces.json` in the plugins root. A third-party
// `marketplace add` may not claim one of the reserved (official) names — those
// are owned by the bundled Genesis/Anthropic catalog entries.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plugin::error::{PluginCliError, Result};

/// Names reserved for the official/bundled catalogs. A third-party add using
/// any of these is rejected (§2.1 reserved-name note).
const RESERVED: &[&str] = &[
    "anthropic",
    "anthropics",
    "claude",
    "claude-code",
    "genesis",
];

/// One registered marketplace: a name plus the source spec used to acquire its
/// catalog (a local path or a git URL/`owner/repo`). `official` is set for the
/// bundled entries and exempts them from the reserved-name guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceRef {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub official: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownFile {
    #[serde(default)]
    marketplaces: BTreeMap<String, MarketplaceRef>,
}

fn known_path(plugins_root: &Path) -> PathBuf {
    plugins_root.join("known_marketplaces.json")
}

fn load(plugins_root: &Path) -> Result<KnownFile> {
    let p = known_path(plugins_root);
    if !p.exists() {
        return Ok(KnownFile::default());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(&p)?)?)
}

fn store(plugins_root: &Path, f: &KnownFile) -> Result<()> {
    std::fs::create_dir_all(plugins_root)?;
    let bytes = serde_json::to_vec_pretty(f)?;
    wcore_config::atomic_write(known_path(plugins_root), &bytes)?;
    Ok(())
}

/// Register (or replace) a marketplace. Rejects a reserved name unless the ref
/// is flagged `official`.
pub fn add_marketplace(plugins_root: &Path, m: MarketplaceRef) -> Result<()> {
    if !m.official && RESERVED.contains(&m.name.as_str()) {
        return Err(PluginCliError::ReservedName(m.name));
    }
    let mut f = load(plugins_root)?;
    f.marketplaces.insert(m.name.clone(), m);
    store(plugins_root, &f)
}

/// List all registered marketplaces, ordered by name.
pub fn list_marketplaces(plugins_root: &Path) -> Result<Vec<MarketplaceRef>> {
    Ok(load(plugins_root)?.marketplaces.into_values().collect())
}

/// Look up a marketplace by name.
pub fn get_marketplace(plugins_root: &Path, name: &str) -> Result<Option<MarketplaceRef>> {
    Ok(load(plugins_root)?.marketplaces.remove(name))
}

/// Remove a marketplace. Returns whether it existed.
pub fn remove_marketplace(plugins_root: &Path, name: &str) -> Result<bool> {
    let mut f = load(plugins_root)?;
    let existed = f.marketplaces.remove(name).is_some();
    if existed {
        store(plugins_root, &f)?;
    }
    Ok(existed)
}
