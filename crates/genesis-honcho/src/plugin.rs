//! v0.6.4 Task 2.3 — Plugin / PluginFactory glue for `genesis-honcho`.
//!
//! `GenesisHonchoFactory` is submitted via `inventory::submit!` so it's
//! discoverable through the host-side `PluginLoader::discover` path without
//! any explicit registration in `main()`. `GenesisHoncho::initialize`
//! registers a single `UserModelSpec { backend: "honcho", ... }` against the
//! scoped `UserModelRegistrar`; the manifest declares
//! `register_user_models = true` and no other surfaces.
//!
//! No live HTTP at registration time — the spec is plain data; reification
//! into a `HonchoClient` happens host-side in bootstrap (mock for tests,
//! `live_from_env` when `api_key_env` resolves at runtime).

use std::sync::OnceLock;

use async_trait::async_trait;
use wcore_plugin_api::{
    Plugin, PluginContext, PluginFactory, PluginManifest, PluginResult, UserModelSpec,
};

/// Embedded copy of the plugin's `plugin.toml`. The TOML lives next to
/// `Cargo.toml` so future tooling (publish, audit) can read it without
/// linking the crate; `include_str!` keeps the manifest single-source.
pub const MANIFEST_TOML: &str = include_str!("../plugin.toml");

fn manifest() -> &'static PluginManifest {
    static M: OnceLock<PluginManifest> = OnceLock::new();
    M.get_or_init(|| {
        // SAFETY: `MANIFEST_TOML` is `include_str!` of the committed
        // plugin.toml. Failure here is a checked-in-source bug caught by
        // the per-plugin unit test, never a production runtime condition.
        PluginManifest::from_toml_str(MANIFEST_TOML)
            .expect("genesis-honcho plugin.toml must parse and validate")
    })
}

pub struct GenesisHoncho;

#[async_trait]
impl Plugin for GenesisHoncho {
    fn manifest(&self) -> &PluginManifest {
        manifest()
    }

    async fn initialize(&self, ctx: &mut PluginContext<'_>) -> PluginResult<()> {
        let spec = UserModelSpec {
            name: "honcho".to_string(),
            description: "Honcho user-model backend".to_string(),
            backend: "honcho".to_string(),
            base_url: None,
            api_key_env: Some("HONCHO_API_KEY".to_string()),
            config: serde_json::Value::Null,
        };
        let registry = ctx.user_models.as_mut().ok_or_else(|| {
            wcore_plugin_api::PluginError::HostMisconfiguration {
                plugin: "genesis-honcho".into(),
                surface: "user_models".into(),
            }
        })?;
        registry.register_user_model(spec)?;
        Ok(())
    }
}

pub struct GenesisHonchoFactory;

impl PluginFactory for GenesisHonchoFactory {
    fn name(&self) -> &'static str {
        "genesis-honcho"
    }

    fn build(&self) -> Box<dyn Plugin> {
        Box::new(GenesisHoncho)
    }
}

inventory::submit! { &GenesisHonchoFactory as &dyn PluginFactory }
