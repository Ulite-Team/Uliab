# Authoring a plugin

This is the path `hello-plugin` and `ulite/jvm` follow. It is deliberately
short: most of the contract lives in the WIT file, not in the plugin code.

## Requirements

- Rust with the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`).
- `wit-bindgen` in your crate.
- A checkout of this repository (or at least of `crates/ulb-plugin-sdk`)
  to generate bindings from, and `wasm-tools` to wrap the artifact into a
  component for `uliab run`.

## Crate shape

A plugin is a `cdylib` that generates guest bindings from the SDK's WIT:

```toml
[package]
name = "my-plugin"
edition = "2024"
version = "0.1.0"

[lib]
crate-type = ["cdylib"]

[dependencies]
ulb-plugin-sdk = { path = "../../Uliab/crates/ulb-plugin-sdk" }
wit-bindgen = "0.51"
```

```rust
// Generate the guest bindings from the SDK's single WIT file.
mod bindings {
    wit_bindgen::generate!({
        path: "../../Uliab/crates/ulb-plugin-sdk/plugin.wit",
        world: "plugin",
    });
}

use bindings::{exports::ulite::ulb::manifest::Guest as Manifest, ...};

struct MyPlugin;

// `export!` is wasm-only: the host build of a plugin should still compile.
#[cfg(target_arch = "wasm32")]
export!(MyPlugin);
```

## The manifest

```rust
fn manifest() -> PluginManifest {
    PluginManifest {
        name: "ulite/my-plugin".into(),   // the registry coordinate
        version: env!("CARGO_PKG_VERSION").into(),
        abi_version: ulb_plugin_sdk::ABI_VERSION.to_string(),
        tools: vec!["javac".into(), "jar".into()],
    }
}
```

Rules:

- `name` must be the coordinate the registry resolves (host checks it).
- `abi_version` must be the SDK constant — never a hand-typed literal. The
  host compares it to its own constant; a hand-typed value that drifts is
  caught at load time.
- `tools` must list every tool any `run-tool` task will request. If you
  declare none and register a `run-tool` task, the host rejects it.

## `configure`

`configure` receives the module's resolved config block as JSON, validates
its own slice of it, and registers tasks:

```rust
fn configure(module_config: String) -> Result<(), String> {
    let config: Value = serde_json::from_str(&module_config)
        .map_err(|e| format!("invalid module config JSON: {e}"))?;

    let project_dir = config["projectDir"].as_str()
        .ok_or("module config is missing 'projectDir'")?;

    registrar.register_task(Task {
        name: "assemble".into(),
        inputs: vec![classes_dir.into()],
        outputs: vec![jar_file.into()],
        depends_on: vec!["compile".into()],
        action: Action::RunTool(RunToolArgs {
            tool: AllowlistedTool::Jar,
            args: vec!["cf".into(), jar_file.into(), "-C".into(), classes_dir.into(), ".".into()],
            cwd: project_dir.into(),
        }),
    })?;
    Ok(())
}
```

Every relative path in a task should resolve against the injected
`projectDir`, never against the host's current working directory — the
host does not guarantee where it was invoked from.

## Registering tasks

- `register-task` only succeeds inside `configure`. The host unbinds the
  registrar afterwards.
- Task `depends_on` names are module-local sibling names. The host
  validates the assembled graph (undefined references, cycles) after
  `configure` returns.
- Choose one `action` per task: `copy-file`, `run-tool`, or `write-file`.
  `write-file` is how a plugin synthesizes source (the jvm plugin's
  generated test runner does exactly this).
- Declare declared-but-unused tools or leave `tools` empty when a plugin
  registers no tasks; `configure` should still validate the config JSON so
  malformed modules fail loudly.

## Building and verifying

```sh
cargo build -p my-plugin --release --target wasm32-wasip2
wasm-tools component new target/wasm32-wasip2/release/my_plugin.wasm \
  -o my_plugin.wasm
uliab run my_plugin.wasm 'input'
```

The `uliab run` output includes the parsed manifest, so you can see the
reported `name`/`version`/`abi-version`/`tools` on stdout. A component
whose `abi-version` does not match the host is refused before any plugin
code runs — that check is the point of the manifest.

See `crates/ulb-plugin-fixture` and `crates/ulb-plugin-legacy-fixture` in
this repo, and `hello-plugin`/`jvm-plugin` in `Ulite-Team/ulb-plugins`,
for complete working examples.
