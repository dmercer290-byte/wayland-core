// Lane C: marketplace catalog parsing + the resolve→clone→lower→plan→commit
// install pipeline.
//
// Foreign-format knowledge (the Claude Code `marketplace.json` schema) lives in
// `parse_marketplace`; everything past `detect_format` is format-blind and
// flows through the `wcore-pluginsrc` adapters. Nothing here spawns a process
// or writes to the plugin store until `commit_install` is called — planning is
// pure (the InstallPlan is the consent surface).

use std::path::{Path, PathBuf};

use serde_json::Value;
use wcore_pluginsrc::{
    CanonicalDraft, CommitMeta, InstallPlan, ResolvedVersion, SourceEntry, SourceKind, commit_plan,
    detect_format,
};

use crate::plugin::error::{PluginCliError, Result};
use crate::plugin::{known, lockfile, quarantine};

/// Top-level metadata from a `.claude-plugin/marketplace.json` catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceMeta {
    pub name: String,
    pub owner_name: Option<String>,
    pub owner_email: Option<String>,
    /// `metadata.pluginRoot` — base dir prepended to relative-path sources.
    pub plugin_root: Option<String>,
}

/// Parse a `marketplace.json` body into its metadata and the normalized source
/// list. `metadata.pluginRoot` is prepended to every relative-path source. Any
/// `..` in a relative path, git-subdir `path`, or `pluginRoot` is rejected with
/// [`PluginCliError::PathTraversal`] before it can reach a clone or copy.
pub fn parse_marketplace(json: &str) -> Result<(MarketplaceMeta, Vec<SourceEntry>)> {
    let root: Value = serde_json::from_str(json)?;
    let obj = root.as_object().ok_or_else(|| {
        PluginCliError::Quarantine("marketplace.json: top-level is not an object".into())
    })?;

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginCliError::Quarantine("marketplace.json: missing 'name'".into()))?
        .to_string();

    let owner = obj.get("owner").and_then(Value::as_object);
    let owner_name = owner
        .and_then(|o| o.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let owner_email = owner
        .and_then(|o| o.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let plugin_root = obj
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| m.get("pluginRoot"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(pr) = &plugin_root {
        reject_traversal(pr)?;
    }

    let plugins = obj
        .get("plugins")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PluginCliError::Quarantine("marketplace.json: missing 'plugins' array".into())
        })?;

    let mut entries = Vec::with_capacity(plugins.len());
    for p in plugins {
        let pe = p.as_object().ok_or_else(|| {
            PluginCliError::Quarantine("marketplace.json: plugin entry is not an object".into())
        })?;
        let pname = pe
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginCliError::Quarantine("marketplace.json: plugin entry missing 'name'".into())
            })?
            .to_string();
        // Claude Code defaults `strict` to true.
        let strict = pe.get("strict").and_then(Value::as_bool).unwrap_or(true);
        let declared_version = pe
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string);
        let source = pe.get("source").ok_or_else(|| {
            PluginCliError::Quarantine(format!(
                "marketplace.json: plugin '{pname}' missing 'source'"
            ))
        })?;
        let kind = parse_source(source, plugin_root.as_deref())?;
        entries.push(SourceEntry {
            name: pname,
            kind,
            strict,
            declared_version,
        });
    }

    Ok((
        MarketplaceMeta {
            name,
            owner_name,
            owner_email,
            plugin_root,
        },
        entries,
    ))
}

/// Map one `source` field (a bare string = relative path, or an object with a
/// `source` discriminator) to a [`SourceKind`].
fn parse_source(source: &Value, plugin_root: Option<&str>) -> Result<SourceKind> {
    if let Some(s) = source.as_str() {
        reject_traversal(s)?;
        let joined = match plugin_root {
            Some(root) => format!(
                "{}/{}",
                root.trim_end_matches('/'),
                s.trim_start_matches("./")
            ),
            None => s.to_string(),
        };
        return Ok(SourceKind::RelativePath(PathBuf::from(joined)));
    }

    let obj = source.as_object().ok_or_else(|| {
        PluginCliError::Quarantine(
            "marketplace.json: source is neither a string nor an object".into(),
        )
    })?;
    let ty = obj.get("source").and_then(Value::as_str).ok_or_else(|| {
        PluginCliError::Quarantine(
            "marketplace.json: source object missing 'source' discriminator".into(),
        )
    })?;

    let get = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let require = |k: &str| {
        get(k).ok_or_else(|| {
            PluginCliError::Quarantine(format!("marketplace.json: '{ty}' source missing '{k}'"))
        })
    };

    match ty {
        "github" => {
            let repo = require("repo")?;
            reject_traversal(&repo)?;
            Ok(SourceKind::Github {
                repo,
                git_ref: get("ref"),
                sha: get("sha"),
            })
        }
        "url" => Ok(SourceKind::Url {
            url: require("url")?,
            git_ref: get("ref"),
            sha: get("sha"),
        }),
        "git-subdir" => {
            let path = require("path")?;
            reject_traversal(&path)?;
            Ok(SourceKind::GitSubdir {
                url: require("url")?,
                path,
                git_ref: get("ref"),
                sha: get("sha"),
            })
        }
        "npm" => Ok(SourceKind::Npm {
            package: require("package")?,
            version: get("version"),
            registry: get("registry"),
        }),
        other => Err(PluginCliError::Quarantine(format!(
            "marketplace.json: unknown source type '{other}'"
        ))),
    }
}

/// Reject any path that is absolute or contains a `..` (parent-dir) component.
/// Shared by the parser and the quarantine clone. Rejecting absolute/root/prefix
/// components matters because `Path::join` REPLACES its base when the argument
/// is absolute — `clone_dir.join("/etc")` would escape the clone entirely on
/// Unix, and a `C:\…` prefix does the same on Windows. The source string is
/// attacker-controlled (it comes straight from `marketplace.json`).
pub(crate) fn reject_traversal(s: &str) -> Result<()> {
    use std::path::Component;
    let p = Path::new(s);
    if p.is_absolute() {
        return Err(PluginCliError::PathTraversal(s.to_string()));
    }
    let bad = p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if bad {
        return Err(PluginCliError::PathTraversal(s.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// C4 — resolve → clone → lower → plan → commit
// ---------------------------------------------------------------------------

/// The result of planning an install: a pure [`InstallPlan`] (the consent
/// surface) plus everything `commit_install` needs to write the store. Holds
/// the lowered draft so the commit step never re-fetches or re-lowers.
pub struct PlannedInstall {
    pub plan: InstallPlan,
    pub draft: CanonicalDraft,
    pub fetched_root: PathBuf,
    pub resolved_sha: Option<String>,
    pub format: String,
    pub source_desc: String,
    pub marketplace: String,
}

/// Resolve `plugin@market` to an [`InstallPlan`]. Writes NOTHING to the plugin
/// store and spawns no plugin process — it only acquires the source into the
/// quarantine and lowers it. The returned plan is what a user approves before
/// `commit_install` mutates disk.
pub fn resolve_and_plan(
    plugins_root: &Path,
    quarantine_root: &Path,
    market: &str,
    plugin: &str,
) -> Result<PlannedInstall> {
    let mref = known::get_marketplace(plugins_root, market)?
        .ok_or_else(|| PluginCliError::MarketplaceNotFound(market.to_string()))?;

    let market_root = acquire_marketplace(&mref, quarantine_root)?;
    let mjson = std::fs::read_to_string(market_root.join(".claude-plugin/marketplace.json"))
        .map_err(|_| {
            PluginCliError::Quarantine(format!(
                "no .claude-plugin/marketplace.json in marketplace '{market}'"
            ))
        })?;
    let (_meta, entries) = parse_marketplace(&mjson)?;
    let entry = entries
        .into_iter()
        .find(|e| e.name == plugin)
        .ok_or_else(|| PluginCliError::NotInRegistry(format!("{plugin}@{market}")))?;

    let (fetched_root, resolved_sha) = match &entry.kind {
        SourceKind::RelativePath(p) => {
            let root = market_root.join(p);
            ensure_within(&market_root, &root)?;
            (root, None)
        }
        other => {
            let qdir = quarantine_root.join(sanitize(&format!("{market}__{plugin}")));
            let cloned = quarantine::quarantine_clone(other, &qdir)?;
            (cloned.path, Some(cloned.resolved_sha))
        }
    };

    let format = detect_format(&fetched_root).ok_or_else(|| {
        PluginCliError::Quarantine(format!(
            "unrecognized plugin format at {}",
            fetched_root.display()
        ))
    })?;
    let adapter = adapter_for(&format)?;
    let draft = adapter.lower(market, &entry, &fetched_root)?;

    let store_path = plugins_root.join(format!("{}@{market}", draft.name));
    let plan = InstallPlan::from_draft(&draft, market, store_path);

    Ok(PlannedInstall {
        plan,
        draft,
        fetched_root,
        resolved_sha,
        format,
        source_desc: quarantine::describe_source(&entry.kind),
        marketplace: market.to_string(),
    })
}

/// Commit a previously-planned install: write the self-contained native plugin
/// dir, then append a commit-pinned lockfile record. `installed_at` is supplied
/// by the caller — lib code never reads the wall clock (keeps this resumable
/// and testable).
pub fn commit_install(
    plugins_root: &Path,
    planned: &PlannedInstall,
    installed_at: String,
) -> Result<PathBuf> {
    let meta = CommitMeta {
        marketplace: &planned.marketplace,
        format: &planned.format,
        resolved_sha: planned.resolved_sha.clone(),
    };
    let dir = commit_plan(&planned.draft, &meta, &planned.fetched_root, plugins_root)?;

    lockfile::record_install(
        plugins_root,
        lockfile::InstallRecord {
            plugin: planned.draft.name.clone(),
            marketplace: planned.marketplace.clone(),
            source: planned.source_desc.clone(),
            resolved_sha: planned.resolved_sha.clone(),
            version: version_string(&planned.draft.version),
            grade: format!("{:?}", planned.plan.grade),
            installed_at,
        },
    )?;

    Ok(dir)
}

/// Make a marketplace's catalog available on disk. A local-path source is read
/// in place; any other source string is treated as a git URL and quarantine-
/// cloned. Returns the directory that contains `.claude-plugin/marketplace.json`.
fn acquire_marketplace(mref: &known::MarketplaceRef, quarantine_root: &Path) -> Result<PathBuf> {
    let local = Path::new(&mref.source);
    if local.is_dir() {
        return Ok(local.to_path_buf());
    }
    let kind = SourceKind::Url {
        url: mref.source.clone(),
        git_ref: None,
        sha: None,
    };
    let qdir = quarantine_root.join(sanitize(&format!("mkt__{}", mref.name)));
    Ok(quarantine::quarantine_clone(&kind, &qdir)?.path)
}

fn adapter_for(format: &str) -> Result<Box<dyn wcore_pluginsrc::PluginFormatAdapter>> {
    match format {
        "claude-code" => Ok(Box::new(wcore_pluginsrc::claude_code::ClaudeCodeAdapter)),
        other => Err(PluginCliError::Quarantine(format!(
            "no install-time adapter for format '{other}'"
        ))),
    }
}

fn version_string(v: &ResolvedVersion) -> String {
    match v {
        ResolvedVersion::Explicit(s) => s.clone(),
        ResolvedVersion::CommitSha(s) => format!("sha:{s}"),
        ResolvedVersion::Unknown => "unknown".to_string(),
    }
}

/// Confirm `candidate` does not escape `root` after symlink resolution.
fn ensure_within(root: &Path, candidate: &Path) -> Result<()> {
    let rc = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cc = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if !cc.starts_with(&rc) {
        return Err(PluginCliError::PathTraversal(
            candidate.display().to_string(),
        ));
    }
    Ok(())
}

/// Sanitize a string for use as a single on-disk directory component.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
