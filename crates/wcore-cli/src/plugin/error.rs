// M5.4: typed error surface for the plugin marketplace subcommand.
// Every fallible code path in this module returns `Result<T,
// PluginCliError>` so the CLI dispatcher can map specific variants to
// non-zero exit codes / friendly messages without string-matching.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginCliError {
    #[error("plugin not in registry: {0}")]
    NotInRegistry(String),

    #[error("plugin already installed: {0}")]
    AlreadyInstalled(String),

    #[error("plugin not installed: {0}")]
    NotInstalled(String),

    #[error("manifest parse: {0}")]
    ManifestParse(#[from] toml::de::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Plugin name failed the canonical kebab-case validation. Always
    /// returned BEFORE the name is interpolated into any path or URL.
    /// See `resolver::validate_plugin_name` for the exact rules.
    #[error("invalid plugin name: {0} (must match ^[a-z][a-z0-9-]*$)")]
    InvalidName(String),

    /// Generic transport / network failure for the remote registry path.
    /// Kept as `String` so the variant compiles regardless of whether
    /// the `remote-registry` feature is enabled.
    #[error("network: {0}")]
    Network(String),

    #[error("no release asset found for plugin {plugin} on {host}")]
    NoReleaseAsset { plugin: String, host: String },

    /// A source path (relative path, git-subdir `path`, repo, or
    /// `metadata.pluginRoot`) contained a `..` component or otherwise
    /// resolved outside its containment root. Rejected before any clone or
    /// copy touches disk.
    #[error("path traversal rejected in source: {0}")]
    PathTraversal(String),

    /// A `git` invocation in the quarantine clone failed, timed out, or
    /// produced no resolvable commit.
    #[error("git: {0}")]
    Git(String),

    /// The quarantine acquisition layer rejected something (unsupported
    /// source shape, size-cap overrun, missing marketplace.json, flag-like
    /// argument, unrecognized plugin format).
    #[error("quarantine: {0}")]
    Quarantine(String),

    /// A named marketplace is not present in `known_marketplaces.json`.
    #[error("marketplace not found: {0}")]
    MarketplaceNotFound(String),

    /// A third-party `marketplace add` tried to claim one of the reserved
    /// (official) marketplace names.
    #[error("reserved marketplace name: {0}")]
    ReservedName(String),

    /// Lowering a foreign plugin into the Genesis-native model failed.
    #[error("plugin lowering: {0}")]
    PluginSrc(#[from] wcore_pluginsrc::PluginSrcError),
}

pub type Result<T> = std::result::Result<T, PluginCliError>;
