# The host (`crates/uliab/src/host.rs`)

`PluginHost` embeds wasmtime's component runtime. Its bindings are
generated with `wasmtime::component::bindgen!` against
`ulb_plugin_sdk::WIT` — the exact same text plugins bind against — so the
host and any plugin are compiled from one interface.

## Loading

`PluginHost::manifest_of_bytes(wasm)` instantiates a component and calls
its `manifest` export. This is how the registry client verifies an
artifact before caching it, and how `uliab run` drives a raw file.

Two load shapes exist:

| Type | World | Entry points |
|---|---|---|
| `LoadedPlugin` | current `plugin` world | `manifest`, `configure` |
| `LoadedLegacyPlugin` | frozen `legacy-plugin.wit` (ABI 0.1) | `manifest`, `run` |

The legacy path exists so a pre-`configure` component still instantiates
and runs after the ABI grew. Instantiation is done against the legacy
world because world-level instantiation requires every export; a
pre-`configure` component lacks `configure`, and a new component satisfies
the legacy world because extra exports are ignored.

## The ABI check

Before any plugin code runs, `check_abi` compares the manifest's
`abi-version` to `ulb_plugin_sdk::ABI_VERSION`. Today it is an **exact
match**:

```
plugin abi-version '0.3' does not match host abi-version '0.4'
```

A later ABI major widens this to the compatible-range rule described in
`architecture.md` §3.6.

## Capability state during `configure`

`HostCtx` carries a `WasiView` (context plus resource table) and a
`registrar` slot:

```
struct HostCtx {
    wasi: WasiCtx,
    table: ResourceTable,
    registrar: Option<RegistrarState>,
}
```

`RegistrarState` holds the tasks being collected plus the module name and
the plugin's declared tools:

```
struct RegistrarState {
    graph: TaskGraph,
    module: String,
    declared_tools: HashSet<AllowlistedTool>,
}
```

The `registrar` slot is `None` outside `configure`, so a
`register-task` call from anywhere else is refused by construction. Inside
`configure` the host implements the `task-registrar` import:

- rejects a task whose tool is not allowlisted, or not declared in the
  plugin's manifest;
- rejects duplicate task names within the module;
- after `configure` returns, re-validates the assembled graph for
  undefined `depends_on` references and cycles before returning it to the
  driver.

The registrar host impl is bound into the linker for every instantiation.

## wasi imports

A pure-compute plugin still imports `wasi:io/poll` and friends because the
`wasm32-wasip2` std runtime links them in. The host registers its
`WasiView` before instantiating a component, so a plugin loads without
adapter shims.

## The driver hand-off

`host.rs` is the low-level piece. The orchestration — which plugins to
load for a project, which config JSON to hand each one, and what to do
with the registered task graphs — lives in [build-pipeline.md](build-pipeline.md).
