//! Hook-world bindings (Task 2.3).
//!
//! Generated from `wit/hook.wit`. The world `genesis-hook` imports the shared
//! `genesis:host/host` interface and exports the `hook` interface.

wasmtime::component::bindgen!({
    path: "wit",
    world: "genesis-hook",
    imports: { default: async },
    exports: { default: async },
});
