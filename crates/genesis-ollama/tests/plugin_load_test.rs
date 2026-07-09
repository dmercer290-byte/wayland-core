//! W8a B.3 — plugin discovery via inventory + manifest validation.
//!
//! Confirms the `GenesisOllamaFactory` is submitted to the inventory
//! collection (so `PluginLoader::discover` finds it without explicit
//! registration) and that the embedded `plugin.toml` parses + passes
//! the wcore-plugin-api manifest schema validator.

use wcore_plugin_api::{Plugin, PluginFactory, PluginManifest};

use genesis_ollama::{GenesisOllama, GenesisOllamaFactory, MANIFEST_TOML};

#[test]
fn plugin_manifest_parses_and_validates() {
    let manifest =
        PluginManifest::from_toml_str(MANIFEST_TOML).expect("manifest must parse + validate");
    assert_eq!(manifest.plugin.name, "genesis-ollama");
    assert_eq!(manifest.plugin.version, "0.1.0");
    assert!(manifest.permissions.register_providers);
    assert!(!manifest.permissions.register_tools);
    assert!(!manifest.permissions.register_hooks);
    assert!(!manifest.permissions.register_agents);
    assert!(!manifest.permissions.register_skills);
    assert!(!manifest.permissions.register_rules);
    assert!(!manifest.permissions.register_mcp_server);
}

#[test]
fn factory_name_matches_manifest_name() {
    let factory = GenesisOllamaFactory;
    assert_eq!(factory.name(), "genesis-ollama");
    let plugin = factory.build();
    let manifest = plugin.manifest();
    assert_eq!(manifest.plugin.name, factory.name());
}

#[test]
fn plugin_factory_is_discoverable_via_inventory() {
    // Walk every PluginFactory submitted to the inventory and confirm
    // genesis-ollama appears exactly once. The discovery path is what
    // `PluginLoader::discover` consumes at runtime.
    let mut count_ollama = 0;
    for factory in inventory::iter::<&'static dyn PluginFactory> {
        if factory.name() == "genesis-ollama" {
            count_ollama += 1;
        }
    }
    assert_eq!(
        count_ollama, 1,
        "genesis-ollama must be submitted exactly once via inventory::submit!"
    );
}

#[test]
fn genesisollama_struct_is_constructible_directly() {
    // Sanity: the plugin type is public so downstream test harnesses can
    // construct it without going through PluginFactory if they want.
    let plugin = GenesisOllama;
    let m = plugin.manifest();
    assert_eq!(m.plugin.name, "genesis-ollama");
}

// Note: end-to-end `initialize(&mut PluginContext)` registration is
// covered by the wcore-agent host-side plugin smoke tests; the host
// adapters live in wcore-agent and importing them from genesis-ollama
// would invert the dep graph. The factory-name + inventory-discovery
// checks above are sufficient to gate the W2.5 register_providers path
// from this crate's perspective.
