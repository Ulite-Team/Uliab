# The plugin ABI

The ABI is defined in `crates/ulb-plugin-sdk/plugin.wit` (package
`ulite:ulb`). The host and every plugin generate their bindings from that
one file — the host with `wasmtime::component::bindgen!`, plugins with
`wit_bindgen::generate!` pointing at the same path — so there is no second
copy of the interface to keep in sync.

The current ABI version is `ulb_plugin_sdk::ABI_VERSION == "0.7"`.
A plugin reports this value verbatim in its manifest; the host reads it
back and refuses to run anything that disagrees, before executing any
plugin code.

## World

```
world plugin {
  export manifest;
  export configure;
  import task-registrar;
}
```

### `manifest`

```
record plugin-manifest {
  name: string,
  version: string,
  abi-version: string,
  tools: list<string>,
  dependencies: list<string>,
}

export manifest: func() -> plugin-manifest;
```

The host reads this on every load and cross-checks it:

- `name` must equal the registry coordinate the plugin was resolved as
  (e.g. `ulite/jvm`). This is what makes pre-cache verification meaningful.
- `version` is the plugin's own semver (from `CARGO_PKG_VERSION`).
- `abi-version` is `ulb_plugin_sdk::ABI_VERSION`; the host requires an exact
  match. A later ABI major widens this to a compatible-range check.
- `tools` lists every tool the plugin will request through `run-tool`.
  The host rejects any `run-tool` task whose tool was not declared here.
- `dependencies` lists plugin names this plugin requires at configure time.
  The host validates that every declared dependency is present in the build;
  a missing dependency is a configure-time error.

### `task-registrar`

```
enum allowlisted-tool { cp, cat, mkdir, echo, javac, kotlinc, jar, java, aapt2, apksigner }

record copy-args     { source: string, destination: string }
record run-tool-args { tool: allowlisted-tool, args: list<string>, cwd: string }
record write-file-args { path: string, contents: string }

variant action {
  copy-file(copy-args),
  run-tool(run-tool-args),
  write-file(write-file-args),
}

record task {
  name: string,
  inputs: list<string>,
  outputs: list<string>,
  depends-on: list<string>,
  action: action,
}

register-task: func(task) -> result<_, string>;
```

`task-registrar` is an **import**: the host implements it. A plugin can
only register tasks, and only inside its `configure` call. The action set
is closed on purpose — plugins get no ambient filesystem, network, or
process access; every external tool (`javac`, `kotlinc`, `jar`, `java`,
`aapt2`, `apksigner`, and the unix coreutils) is spawned by the host
through the allowlisted `run-tool` capability. `aapt2` and `apksigner`
are resolved under the `build-tools` directory the action names as its
first argument, because they ship inside the Android SDK rather than on
the `PATH`; the host strips that directory before passing the remaining
arguments to the tool.

`register-task` errors on:

- a task name that already exists in the module;
- a task name containing a colon (`:`), which is reserved for cross-plugin
  dependency references;
- an action tool that is not in the allowlist;
- an action tool that is not declared in the plugin's manifest `tools`.

### `configure`

```
import task-registrar;
export configure: func(module-config: string) -> result<_, string>;
```

`configure` is the plugin's entire build surface. It receives the
module's resolved config block serialized as JSON (see
[build-pipeline.md](build-pipeline.md) for exactly what the host injects),
validates its own keys, registers tasks, and returns. Tasks registered
outside `configure` are impossible: the host only binds the registrar
state during that call.

## Cross-plugin dependencies

A plugin can declare that it depends on another plugin's tasks by listing
the other plugin's name in its manifest `dependencies` field and using the
`"plugin_name:task_name"` format (single colon) in a task's `depends_on`
entries. For example, a KMP plugin that needs the Android plugin's
`compileKotlinDebug` task would register:

```json
{
  "name": "ulite/kmp",
  "dependencies": ["ulite/android"],
  ...
}
```

And register a task with:

```json
{
  "name": "compileCommon",
  "depends_on": ["ulite/android:compileKotlinDebug"],
  ...
}
```

The host resolves cross-plugin references at graph-merge time, after all
plugins have configured. The driver builds a `plugin → task names` index,
validates that every declared dependency is present in the build, and
validates that every cross-plugin reference names a plugin listed in the
consumer's declared `dependencies` — an undeclared reference is a build
error. Finally it rewrites `"plugin:task"` entries to bare task names in
the merged graph. Same-module deps (bare names) are validated per-plugin
during `configure`; cross-plugin deps are deferred to the driver where all
tasks are visible. Task names must not contain colons (`:`) — the colon
is reserved as the cross-plugin reference separator.

## ABI versioning policy

- The ABI is `major.minor`, independent of the tool's own version.
- Growth is **additive only** within a major version: adding an entry
  point, extending the allowlist, or adding an action variant must never
  break a component built against an older ABI of the same major.
- Breaking changes bump the major and freeze the previous world as a
  snapshot the host keeps bindings to.

That last rule is already exercised: ABI 0.1 was `manifest` + `run` only
(no `configure`, no registrar). It is frozen as
`crates/ulb-plugin-sdk/legacy-plugin.wit`, and the host keeps a
`LoadedLegacyPlugin` path so a 0.1 component still instantiates and runs
after every subsequent growth. A new component satisfies the legacy world
because extra exports are ignored.

## Manifest tools vs the ABI's own allowlist

Two different sets, don't confuse them:

- The WIT `allowlisted-tool` enum is the closed universe of tools the host
  will ever spawn.
- The manifest's `tools` field is a plugin's **declared** subset of that
  universe, enforced at execution time.

`architecture.md` §3.5 and the `jvm-plugin`'s manifest (`javac`, `kotlinc`,
`jar`, `java`) are the reference example of the two working together.
