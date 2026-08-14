//! A test plugin built against the pre-`configure` ABI (the frozen
//! `legacy-plugin.wit` in the sdk crate): it exports `manifest` and `run`
//! but does not import `task-registrar`.
//!
//! The `uliab` integration tests build this crate for `wasm32-wasip2` and
//! check that [`uliab::host::PluginHost`] still loads and runs it after
//! the ABI grew the `configure` entry point and the `task-registrar`
//! import.
#![allow(unsafe_code)]

wit_bindgen::generate!({
    path: "../ulb-plugin-sdk/legacy-plugin.wit",
    world: "plugin",
});

/// The legacy fixture: identity plus the trivial `run` passthrough.
struct Legacy;

impl exports::ulite::ulb::ulb_plugin::Guest for Legacy {
    fn manifest() -> exports::ulite::ulb::ulb_plugin::PluginManifest {
        exports::ulite::ulb::ulb_plugin::PluginManifest {
            name: "ulite/legacy".to_owned(),
            version: "0.1.0".to_owned(),
            abi_version: "0.1".to_owned(),
        }
    }

    fn run(input: String) -> String {
        input
    }
}

#[cfg(target_arch = "wasm32")]
export!(Legacy);
