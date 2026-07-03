//! Plugin / PluginFactory glue for the genesis-ollama reference plugin.
//!
//! `GenesisOllamaFactory` is submitted via `inventory::submit!` so it's
//! discoverable through the host-side `PluginLoader::discover` path
//! without any explicit registration in main(). `GenesisOllama::initialize`
//! registers a single `OllamaProvider` against the scoped registry; the
//! manifest declares `register_providers = true` and no other surfaces.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use wcore_plugin_api::registry::config::ScopedConfigReader;
use wcore_plugin_api::{Plugin, PluginContext, PluginFactory, PluginManifest, PluginResult};

use crate::provider::OllamaProvider;

/// Hardcoded fallbacks used when neither host config nor env supply a value.
/// These match the historical defaults baked into `initialize()`.
const DEFAULT_BASE_URL: &str = "http://localhost:11434/api/chat";
const DEFAULT_MODEL: &str = "llama3";

/// Host config keys (flat) read from `ctx.config`.
const CONFIG_KEY_BASE_URL: &str = "ollama_base_url";
const CONFIG_KEY_MODEL: &str = "ollama_model";

/// Environment variable overrides (take precedence over config, which itself
/// takes precedence over the hardcoded fallback).
const ENV_BASE_URL: &str = "OLLAMA_BASE_URL";
const ENV_MODEL: &str = "OLLAMA_MODEL";

/// Resolve `base_url` and `model` with precedence env > host config > default.
///
/// `getenv` is injected so tests can exercise the env tier without mutating
/// process-global state (`std::env::set_var` is `unsafe` on edition 2024 and
/// races across parallel tests).
fn resolve_provider_settings(
    config: &ScopedConfigReader<'_>,
    getenv: impl Fn(&str) -> Option<String>,
) -> (String, String) {
    let base_url = getenv(ENV_BASE_URL)
        .or_else(|| config.get::<String>(CONFIG_KEY_BASE_URL))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let model = getenv(ENV_MODEL)
        .or_else(|| config.get::<String>(CONFIG_KEY_MODEL))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    (base_url, model)
}

/// Embedded copy of the plugin's `plugin.toml`. The TOML lives next to
/// `Cargo.toml` so future tooling (publish, audit) can read it without
/// linking the crate; `include_str!` keeps the manifest single-source.
pub const MANIFEST_TOML: &str = include_str!("../plugin.toml");

fn manifest() -> &'static PluginManifest {
    static M: OnceLock<PluginManifest> = OnceLock::new();
    M.get_or_init(|| {
        // SAFETY: `MANIFEST_TOML` is `include_str!` of the committed
        // plugin.toml. Failure here is a checked-in-source bug caught
        // by the per-plugin unit test, never a production runtime
        // condition.
        PluginManifest::from_toml_str(MANIFEST_TOML)
            .expect("genesis-ollama plugin.toml must parse and validate")
    })
}

pub struct GenesisOllama;

#[async_trait]
impl Plugin for GenesisOllama {
    fn manifest(&self) -> &PluginManifest {
        manifest()
    }

    async fn initialize(&self, ctx: &mut PluginContext<'_>) -> PluginResult<()> {
        // Resolve endpoint + model from host config / env, falling back to the
        // historical hardcoded defaults when unset (env > config > default).
        let (base_url, model) = resolve_provider_settings(&ctx.config, |k| std::env::var(k).ok());
        let provider = Arc::new(OllamaProvider::new(base_url, model));
        // Wave RB STABILITY MINOR #13: typed HostMisconfiguration error.
        let registry = ctx.providers.as_mut().ok_or_else(|| {
            wcore_plugin_api::PluginError::HostMisconfiguration {
                plugin: "genesis-ollama".into(),
                surface: "providers".into(),
            }
        })?;
        registry.register_provider(provider)?;
        Ok(())
    }
}

pub struct GenesisOllamaFactory;

impl PluginFactory for GenesisOllamaFactory {
    fn name(&self) -> &'static str {
        "genesis-ollama"
    }

    fn build(&self) -> Box<dyn Plugin> {
        Box::new(GenesisOllama)
    }
}

inventory::submit! { &GenesisOllamaFactory as &dyn PluginFactory }

#[cfg(test)]
mod resolve_tests {
    use super::{
        CONFIG_KEY_BASE_URL, CONFIG_KEY_MODEL, DEFAULT_BASE_URL, DEFAULT_MODEL, ENV_BASE_URL,
        ENV_MODEL, resolve_provider_settings,
    };
    use std::collections::HashMap;
    use wcore_plugin_api::registry::config::{ConfigReader, ScopedConfigReader};

    #[derive(Default)]
    struct FakeHost {
        values: HashMap<String, serde_json::Value>,
    }

    impl ConfigReader for FakeHost {
        fn get_raw(&self, key: &str) -> Option<serde_json::Value> {
            self.values.get(key).cloned()
        }
    }

    /// Helper: build a `getenv` closure from an explicit map so tests never
    /// mutate process-global env (which is `unsafe` and racy across threads).
    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn falls_back_to_hardcoded_defaults_when_unset() {
        let host = FakeHost::default();
        let reader = ScopedConfigReader::new(&host);
        let (base_url, model) = resolve_provider_settings(&reader, |_| None);
        assert_eq!(base_url, DEFAULT_BASE_URL);
        assert_eq!(model, DEFAULT_MODEL);
    }

    #[test]
    fn host_config_overrides_defaults() {
        let mut host = FakeHost::default();
        host.values.insert(
            CONFIG_KEY_BASE_URL.to_string(),
            serde_json::json!("http://gpu-box:11434/api/chat"),
        );
        host.values
            .insert(CONFIG_KEY_MODEL.to_string(), serde_json::json!("mistral"));
        let reader = ScopedConfigReader::new(&host);
        let (base_url, model) = resolve_provider_settings(&reader, |_| None);
        assert_eq!(base_url, "http://gpu-box:11434/api/chat");
        assert_eq!(model, "mistral");
    }

    #[test]
    fn env_overrides_both_config_and_default() {
        let mut host = FakeHost::default();
        // Config sets one value; env must win over it.
        host.values
            .insert(CONFIG_KEY_MODEL.to_string(), serde_json::json!("mistral"));
        let reader = ScopedConfigReader::new(&host);
        let getenv = env_from(&[
            (ENV_BASE_URL, "http://remote:9999/api/chat"),
            (ENV_MODEL, "qwen2"),
        ]);
        let (base_url, model) = resolve_provider_settings(&reader, getenv);
        assert_eq!(base_url, "http://remote:9999/api/chat");
        assert_eq!(model, "qwen2");
    }
}
