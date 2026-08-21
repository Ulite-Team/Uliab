//! A test plugin that declares a cross-plugin dependency on `ulite/fixture`.
//!
//! Given a `consumeFrom` config key (e.g. `"ulite/fixture:stage"`), it
//! registers a task whose `depends_on` contains that cross-plugin
//! reference. The host resolves it at graph-merge time, proving the ABI
//! supports plugin-to-plugin task ordering.
#![allow(unsafe_code)]

wit_bindgen::generate!({
    path: "../ulb-plugin-sdk/plugin.wit",
    world: "plugin",
});

use ulite::ulb::task_registrar::{self, Action, AllowlistedTool, RunToolArgs, Task};

struct CrossDepFixture;

impl exports::ulite::ulb::ulb_plugin::Guest for CrossDepFixture {
    fn manifest() -> exports::ulite::ulb::ulb_plugin::PluginManifest {
        exports::ulite::ulb::ulb_plugin::PluginManifest {
            name: "ulite/cross-dep-fixture".to_owned(),
            version: "0.1.0".to_owned(),
            abi_version: ulb_plugin_sdk::ABI_VERSION.to_owned(),
            tools: vec!["echo".to_owned()],
            dependencies: vec!["ulite/fixture".to_owned()],
        }
    }

    fn configure(module_config: String) -> Result<(), String> {
        let config: serde_json::Value = serde_json::from_str(&module_config)
            .map_err(|error| format!("invalid module config JSON: {error}"))?;

        let consume_from = config
            .get("consumeFrom")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "module config is missing 'consumeFrom'".to_owned())?
            .to_owned();

        task_registrar::register_task(&Task {
            name: "consume".to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            depends_on: vec![consume_from],
            action: Action::RunTool(RunToolArgs {
                tool: AllowlistedTool::Echo,
                args: vec!["consumed".to_owned()],
                cwd: ".".to_owned(),
            }),
        })?;

        Ok(())
    }

    fn run(input: String) -> String {
        input
    }
}

#[cfg(target_arch = "wasm32")]
export!(CrossDepFixture);
