//! Lane E1 — per-executable MCP spawn-consent key.
//!
//! When a marketplace plugin ships an MCP server, installing it grants consent
//! to spawn *that specific executable with those specific arguments and that
//! specific set of environment-variable keys*. [`spawn_consent_key`] reduces an
//! [`McpServerSpec`] to a stable hash of exactly those things so that:
//!
//!   * the installer (`wcore-cli`) can record the granted key in a sidecar, and
//!   * the runtime loader (`wcore-agent`) can recompute the key and refuse to
//!     spawn a server whose command, args, transport, or env-key set changed
//!     since consent was granted (e.g. after a plugin update).
//!
//! Two deliberate properties:
//!
//!   * **Env VALUES are never hashed** — only the sorted, de-duplicated set of
//!     env *keys*. Values routinely carry secrets and machine-specific absolute
//!     paths; hashing them would both leak nothing useful into the key and make
//!     it non-portable.
//!   * **The key is computed on the TEMPLATE form** — i.e. before `${VAR}`
//!     substitution (see Lane D `var_subst`). The stored `plugin.toml` holds the
//!     template, and the loader recomputes the key *before* substituting, so the
//!     install-time key and the spawn-time key are byte-identical across
//!     machines.
//!
//! The server `name` is intentionally excluded: renaming a server does not
//! change what it executes, so it should not invalidate consent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mcp_server_spec::{McpServerSpec, McpTransport};

/// Filename of the spawn-consent sidecar written into a plugin's install dir.
pub const CONSENT_SIDECAR: &str = "consent.json";

/// The spawn-consent grant recorded at install time, read back at spawn time.
///
/// Installing a marketplace plugin records the [`spawn_consent_key`] of each
/// MCP server it ships. The runtime loader recomputes the key for the server it
/// is about to spawn (on the pre-substitution template form) and refuses to
/// spawn anything whose key is not in [`mcp_spawn_keys`]. A plugin update that
/// changes the command, args, transport, or env-key set therefore yields a new
/// key that the old sidecar does not grant, and the server is skipped until the
/// user re-installs and re-consents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSpawnConsent {
    /// Granted spawn-consent keys.
    #[serde(default)]
    pub mcp_spawn_keys: Vec<String>,
}

impl McpSpawnConsent {
    /// Path of the sidecar within an install dir.
    pub fn path(install_dir: &Path) -> PathBuf {
        install_dir.join(CONSENT_SIDECAR)
    }

    /// Load the sidecar from an install dir. Returns `Ok(None)` when no sidecar
    /// exists (i.e. consent was never granted), and an error only on malformed
    /// JSON or an unreadable file.
    pub fn load(install_dir: &Path) -> std::io::Result<Option<Self>> {
        let p = Self::path(install_dir);
        match std::fs::read_to_string(&p) {
            Ok(s) => {
                let parsed = serde_json::from_str(&s)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(parsed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `key` is among the granted spawn-consent keys.
    pub fn grants(&self, key: &str) -> bool {
        self.mcp_spawn_keys.iter().any(|k| k == key)
    }
}

/// Stable consent key for an MCP server: a hex SHA-256 over the transport kind,
/// the command/url + ordered args, and the sorted unique set of env keys.
///
/// Identical inputs always yield the same key; a change to the command, an
/// argument (including reordering), the transport kind, or the *set* of env
/// keys yields a different key. Changing only an env *value* — or the server
/// name — does not change the key.
pub fn spawn_consent_key(spec: &McpServerSpec) -> String {
    consent_key_from_parts(&spec.transport, spec.env.keys().map(String::as_str))
}

/// Same key as [`spawn_consent_key`], computed from raw parts. Lets the
/// installer derive a key from a not-yet-`McpServerSpec` source (e.g. the
/// lowered draft) without first materializing a [`McpServerSpec`].
pub fn consent_key_from_parts<'a>(
    transport: &McpTransport,
    env_keys: impl Iterator<Item = &'a str>,
) -> String {
    // Build an array-only JSON preimage. Arrays preserve order under any
    // serde_json feature set (unlike object key order), so the canonical
    // form is deterministic regardless of how the crate is compiled.
    let (kind, parts): (&str, Vec<String>) = match transport {
        McpTransport::Stdio { command, args } => {
            let mut v = Vec::with_capacity(args.len() + 1);
            v.push(command.clone());
            v.extend(args.iter().cloned());
            ("stdio", v)
        }
        McpTransport::Sse { url } => ("sse", vec![url.clone()]),
        McpTransport::Http { url } => ("http", vec![url.clone()]),
    };

    // Sort + de-duplicate env keys so the key is independent of map ordering.
    let mut keys: Vec<&str> = env_keys.collect();
    keys.sort_unstable();
    keys.dedup();

    let preimage = serde_json::json!(["spawn-consent-v1", kind, parts, keys]).to_string();

    let mut hasher = Sha256::new();
    hasher.update(preimage.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stdio(command: &str, args: &[&str], env: &[(&str, &str)]) -> McpServerSpec {
        McpServerSpec {
            name: "srv".into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
            },
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn deterministic_for_identical_specs() {
        let a = stdio("node", &["server.js"], &[("API_KEY", "x")]);
        let b = stdio("node", &["server.js"], &[("API_KEY", "y")]);
        // Same command/args/env-keys, different env VALUE -> identical key.
        assert_eq!(spawn_consent_key(&a), spawn_consent_key(&b));
    }

    #[test]
    fn env_key_set_change_changes_key() {
        let base = stdio("node", &["s.js"], &[("A", "1")]);
        let extra = stdio("node", &["s.js"], &[("A", "1"), ("B", "2")]);
        assert_ne!(spawn_consent_key(&base), spawn_consent_key(&extra));
    }

    #[test]
    fn env_key_order_does_not_matter() {
        let a = stdio("node", &["s.js"], &[("A", "1"), ("B", "2")]);
        let b = stdio("node", &["s.js"], &[("B", "2"), ("A", "1")]);
        assert_eq!(spawn_consent_key(&a), spawn_consent_key(&b));
    }

    #[test]
    fn command_change_changes_key() {
        let a = stdio("node", &["s.js"], &[]);
        let b = stdio("deno", &["s.js"], &[]);
        assert_ne!(spawn_consent_key(&a), spawn_consent_key(&b));
    }

    #[test]
    fn arg_reorder_changes_key() {
        let a = stdio("node", &["--port", "1"], &[]);
        let b = stdio("node", &["1", "--port"], &[]);
        assert_ne!(spawn_consent_key(&a), spawn_consent_key(&b));
    }

    #[test]
    fn transport_kind_change_changes_key() {
        let stdio_spec = McpServerSpec {
            name: "srv".into(),
            transport: McpTransport::Stdio {
                command: "https://h/x".into(),
                args: vec![],
            },
            env: HashMap::new(),
        };
        let http_spec = McpServerSpec {
            name: "srv".into(),
            transport: McpTransport::Http {
                url: "https://h/x".into(),
            },
            env: HashMap::new(),
        };
        assert_ne!(
            spawn_consent_key(&stdio_spec),
            spawn_consent_key(&http_spec)
        );
    }

    #[test]
    fn name_excluded_from_key() {
        let mut a = stdio("node", &["s.js"], &[("A", "1")]);
        let b = stdio("node", &["s.js"], &[("A", "1")]);
        a.name = "completely-different".into();
        assert_eq!(spawn_consent_key(&a), spawn_consent_key(&b));
    }

    #[test]
    fn spec_and_parts_agree() {
        let spec = stdio("node", &["s.js", "--flag"], &[("Z", "1"), ("A", "2")]);
        let via_parts = consent_key_from_parts(&spec.transport, ["A", "Z"].into_iter());
        assert_eq!(spawn_consent_key(&spec), via_parts);
    }

    #[test]
    fn duplicate_env_keys_collapse() {
        // consent_key_from_parts must dedup so an accidental duplicate key in
        // the source iterator can't fork the key from the spec form.
        let spec = stdio("node", &[], &[("A", "1")]);
        let dup = consent_key_from_parts(&spec.transport, ["A", "A"].into_iter());
        assert_eq!(spawn_consent_key(&spec), dup);
    }

    #[test]
    fn sidecar_missing_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(McpSpawnConsent::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn sidecar_roundtrips_and_grants_only_listed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let spec = stdio("node", &["s.js"], &[("API_KEY", "x")]);
        let key = spawn_consent_key(&spec);
        let consent = McpSpawnConsent {
            mcp_spawn_keys: vec![key.clone()],
        };
        std::fs::write(
            McpSpawnConsent::path(dir.path()),
            serde_json::to_string_pretty(&consent).unwrap(),
        )
        .unwrap();

        let loaded = McpSpawnConsent::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, consent);
        assert!(loaded.grants(&key));
        assert!(!loaded.grants("some-other-key"));
    }

    #[test]
    fn sidecar_malformed_is_an_error_not_a_silent_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(McpSpawnConsent::path(dir.path()), "{ not json").unwrap();
        assert!(McpSpawnConsent::load(dir.path()).is_err());
    }
}
