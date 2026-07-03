// M5.4: resolver trait + two concrete impls.
//
// `LocalFileResolver`: reads a TOML manifest from a registry directory.
// `GitHubReleasesResolver`: queries `https://api.github.com/repos/<org>/
// <name>/releases/latest`. Gated behind the `remote-registry` feature.
//
// Security model (methodology #14 T5 — locked from day one):
// - Every public entry validates the plugin name FIRST. Path traversal
//   (`..`, `/`, `\`), uppercase, leading digits, and empty strings are
//   rejected before any path or URL is constructed.
// - GitHub URLs are built via `Url::parse` + `path_segments_mut`, never
//   via string interpolation of the plugin name.
// - The HTTP client carries an explicit 15s timeout and a UA string so
//   GitHub's abuse heuristics don't blackhole us on rate-limit.

use super::error::{PluginCliError, Result};
use super::manifest::PluginManifest;
use url::Url;

/// Validate a plugin name against the canonical kebab-case pattern
/// `^[a-z][a-z0-9-]*$`.
///
/// REJECTS:
/// - empty strings
/// - any name containing `..`, `/`, or `\` (path traversal / separators)
/// - leading non-lowercase-letter (no digits, no `-`, no uppercase)
/// - any non-`[a-z0-9-]` character anywhere
pub fn validate_plugin_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PluginCliError::InvalidName(name.to_string()));
    }
    // Separator / traversal checks BEFORE the per-char loop so the
    // error message points at the high-level issue.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(PluginCliError::InvalidName(name.to_string()));
    }
    let mut chars = name.chars();
    // First char must be lowercase ASCII letter.
    let first = chars.next().expect("name non-empty by guard above");
    if !first.is_ascii_lowercase() {
        return Err(PluginCliError::InvalidName(name.to_string()));
    }
    for c in chars {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if !ok {
            return Err(PluginCliError::InvalidName(name.to_string()));
        }
    }
    Ok(())
}

/// Resolves a plugin name to a fully-parsed manifest. Implementors do
/// the source-specific I/O (file read, HTTP fetch). The CLI dispatcher
/// just calls `resolve_manifest` and hands the result to `install`.
pub trait Resolver {
    fn resolve_manifest(&self, name: &str) -> Result<PluginManifest>;
}

/// Local filesystem resolver. Each plugin is one TOML file at
/// `<registry_dir>/<name>.toml`. Mirrors what we ship as the embedded
/// default registry but lets users point at their own directory via
/// `--registry-dir`.
pub struct LocalFileResolver {
    pub registry_dir: std::path::PathBuf,
}

impl LocalFileResolver {
    pub fn new(registry_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            registry_dir: registry_dir.into(),
        }
    }
}

impl Resolver for LocalFileResolver {
    fn resolve_manifest(&self, name: &str) -> Result<PluginManifest> {
        validate_plugin_name(name)?;
        // Use Path::join for the registry lookup. Even though `name` is
        // already validated above, joining keeps cross-platform path
        // semantics intact and never touches the byte-level path.
        let path = self.registry_dir.join(format!("{name}.toml"));
        let raw = std::fs::read_to_string(&path)
            .map_err(|_| PluginCliError::NotInRegistry(name.to_string()))?;
        let mf: PluginManifest = toml::from_str(&raw)?;
        Ok(mf)
    }
}

/// GitHub Releases resolver. Resolves `<name>` against
/// `https://api.github.com/repos/<org>/<name>/releases/latest`.
///
/// The `org` is taken verbatim from the `--source github://<org>` CLI
/// flag; today's locked default is `FerroxLabs`.
pub struct GitHubReleasesResolver {
    pub org: String,
}

impl GitHubReleasesResolver {
    pub fn new(org: impl Into<String>) -> Self {
        Self { org: org.into() }
    }

    /// Build the releases-latest API URL for a plugin.
    ///
    /// MUST validate the plugin name first; the URL is then constructed
    /// via `Url::parse` + `path_segments_mut`, never via string format
    /// of the plugin name into a URL. This is the load-bearing line for
    /// the M5.8 threat model — DO NOT switch to `format!`.
    pub fn release_api_url(&self, name: &str) -> Result<Url> {
        validate_plugin_name(name)?;
        let mut url = Url::parse("https://api.github.com/").expect("static URL parses");
        url.path_segments_mut()
            .map_err(|_| PluginCliError::Network("cannot mut url path".into()))?
            .extend(&["repos", &self.org, name, "releases", "latest"]);
        Ok(url)
    }
}

#[cfg(feature = "remote-registry")]
impl Resolver for GitHubReleasesResolver {
    fn resolve_manifest(&self, name: &str) -> Result<PluginManifest> {
        // Re-validate even though release_api_url validates too — keeps
        // the invariant explicit at the trait boundary.
        validate_plugin_name(name)?;
        let url = self.release_api_url(name)?;
        // F14: route the GitHub release fetch through `wcore_egress` so the
        // request inherits the SSRF policy (host allowlist, redirect handling)
        // instead of a raw `reqwest::blocking::Client` that bypasses it.
        //
        // `EgressClient` is async, but `resolve_manifest` is a sync trait method
        // that runs INSIDE the ambient tokio runtime (`plugin::run` is called
        // from the async `run()` future). Driving a nested `Runtime` from there
        // would panic ("Cannot start a runtime from within a runtime"). The
        // bridge that is safe in both contexts is a fresh OS thread owning its
        // own runtime — outside any ambient runtime, so `block_on` is always
        // legal (mirrors `provider_keys::egress_get_status`).
        let status = std::thread::spawn(move || -> Result<(u16, serde_json::Value)> {
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| PluginCliError::Network(e.to_string()))?;
            runtime.block_on(async move {
                let client = wcore_egress::EgressClient::builder()
                    .user_agent(concat!("genesis-core/", env!("CARGO_PKG_VERSION")))
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .map_err(|e| PluginCliError::Network(e.to_string()))?;
                let resp = client
                    .get(url.as_str())
                    .send()
                    .await
                    .map_err(|e| PluginCliError::Network(e.to_string()))?;
                let code = resp.status().as_u16();
                let json: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| PluginCliError::Network(e.to_string()))?;
                Ok((code, json))
            })
        })
        .join()
        .map_err(|_| PluginCliError::Network("plugin fetch thread panicked".into()))?;
        let (code, json) = status?;
        if !(200..300).contains(&code) {
            return Err(PluginCliError::NoReleaseAsset {
                plugin: name.to_string(),
                host: "github.com".into(),
            });
        }
        // `tag_name` is GitHub's release tag (e.g. `v0.6.0`); strip the
        // leading `v` so consumers see a SemVer string. If absent we
        // record `0.0.0` rather than failing — a release without a tag
        // shouldn't render the whole install unusable.
        let version = json
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .trim_start_matches('v')
            .to_string();
        Ok(PluginManifest {
            name: name.to_string(),
            version,
            requires_sandbox: false,
            description: json
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            dependencies: vec![],
        })
    }
}
