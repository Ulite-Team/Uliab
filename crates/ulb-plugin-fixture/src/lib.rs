//! A test plugin that registers build tasks through the `configure` entry
//! point.
//!
//! It implements the guest side of the plugin ABI (ARCHITECTURE.md §3.2):
//! given the module's configuration as JSON — `{"source": <file>,
//! "output": <file>}` — it registers a `stage` task that copies the source
//! to the output and an independent `announce` task that runs the
//! allowlisted `echo` tool. When the configuration also carries the
//! host-resolved `classpath` object and a `classpathOutput` path, it
//! registers a `copy-classpath` task that copies the first compile jar
//! there, proving a plugin can consume the jars the host resolved for its
//! `deps {}` block. The `uliab` integration tests build this crate for
//! `wasm32-wasip2` and drive it through [`uliab::host::PluginHost`] to
//! prove configure -> task graph -> execute end to end.
#![allow(unsafe_code)]

wit_bindgen::generate!({
    path: "../ulb-plugin-sdk/plugin.wit",
    world: "plugin",
});

use ulite::ulb::task_registrar::{self, Action, AllowlistedTool, CopyArgs, RunToolArgs, Task};

/// The fixture plugin: a copy task plus an echo task per configuration.
struct Fixture;

impl exports::ulite::ulb::ulb_plugin::Guest for Fixture {
    fn manifest() -> exports::ulite::ulb::ulb_plugin::PluginManifest {
        exports::ulite::ulb::ulb_plugin::PluginManifest {
            name: "ulite/fixture".to_owned(),
            version: "0.1.0".to_owned(),
            abi_version: "0.1".to_owned(),
        }
    }

    fn configure(module_config: String) -> Result<(), String> {
        let config: serde_json::Value = serde_json::from_str(&module_config)
            .map_err(|error| format!("invalid module config JSON: {error}"))?;
        let source = config
            .get("source")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "module config is missing 'source'".to_owned())?
            .to_owned();
        let output = config
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "module config is missing 'output'".to_owned())?
            .to_owned();

        task_registrar::register_task(&Task {
            name: "stage".to_owned(),
            inputs: vec![source.clone()],
            outputs: vec![output.clone()],
            depends_on: Vec::new(),
            action: Action::CopyFile(CopyArgs {
                source: source.clone(),
                destination: output,
            }),
        })?;

        task_registrar::register_task(&Task {
            name: "announce".to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            depends_on: Vec::new(),
            action: Action::RunTool(RunToolArgs {
                tool: AllowlistedTool::Echo,
                args: vec!["staged".to_owned(), source],
                cwd: ".".to_owned(),
            }),
        })?;

        let compile_jar = config
            .get("classpath")
            .and_then(|classpath| classpath.get("compile"))
            .and_then(serde_json::Value::as_array)
            .and_then(|jars| jars.first())
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let classpath_output = config
            .get("classpathOutput")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let (Some(compile_jar), Some(classpath_output)) = (compile_jar, classpath_output) {
            task_registrar::register_task(&Task {
                name: "copy-classpath".to_owned(),
                inputs: Vec::new(),
                outputs: vec![classpath_output.clone()],
                depends_on: Vec::new(),
                action: Action::CopyFile(CopyArgs {
                    source: compile_jar,
                    destination: classpath_output,
                }),
            })?;
        }

        Ok(())
    }

    fn run(input: String) -> String {
        input
    }
}

#[cfg(target_arch = "wasm32")]
export!(Fixture);
