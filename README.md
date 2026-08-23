# uliab

> [!WARNING]
> **Experimental — alpha, not stable.** uliab is not yet usable for real
> builds. The CLI, the DSL, the plugin ABI, and the registry format are all
> subject to breaking changes at any time. Do not depend on any of it for
> production work, and do not vendor the ABI.

A declarative, plugin-driven build tool for the `ulb` DSL.

`uliab` reads a project's `settings.ulb` / `build.ulb` / `conventions.ulb`
/ `libs.ulb` files, evaluates them into a generic module model, and then
does no Java, Kotlin, or Android work of its own. All of that lives in
WASM plugins (`ulite/jvm`, `ulite/android`, `ulite/kmp`, …) loaded through
an embedded wasmtime host. The core keeps the DSL, the task DAG engine,
the Maven resolver, and the plugin host; plugins own the toolchain.

The split exists for one reason: the core stays target-agnostic and the
interface between host and plugin is a single, versioned WIT file that
both sides bind from. There is no `.so` loading, no native ABI drift, and
a plugin has no ambient filesystem, network, or process access — only a
closed set of task actions and allowlisted host-spawned tools.

This is early, active development. All three official plugins are
implemented and published: `ulite/jvm` is the reference, `ulite/android`
covers resources → dex → signed per-variant APKs, and `ulite/kmp` covers
shared/JVM source sets plus an Android target composed from the other two.

## What is built today

- **DSL + evaluator** (`ulb-lang`) — error-recovering lexer/parser,
  conventions/fn/task/apply, `deps {}` scopes, flavors/dimensions,
  deterministic evaluation with hermetic `env()`/`props()` injection.
- **Task engine** — topological waves run in parallel, content-addressed
  fingerprints persisted to `.uliab/state.json`, incremental up-to-date
  checks where a re-run dependency forces its dependents to re-run.
- **Maven resolver** — all seven dependency scopes, transitive expansion
  with highest-version-wins conflict resolution, BOM /
  `dependencyManagement` support, content-addressed cache with sha256
  verification, custom `--repo` sources (`https://`, `file://`, paths).
- **Multi-module builds** — `settings.ulb` declares the module list and
  project-wide repositories; `project(":module")` entries in a `deps {}`
  block wire cross-module classpaths in dependency order.
- **Plugin host + registry client** — wasmtime component host bound from
  the SDK WIT; resolution through the public registry index (or a local
  one) into an ABI-checked cache.
- **Plugin-to-plugin composition** (ABI 0.7) — a plugin's manifest can
  declare dependencies on other plugins and reference their tasks across
  the graph boundary; undeclared references are build errors.
- **`uliab init`** — scaffolds a JVM, Android, or KMP project (sources,
  `.ulb` files included) whose first `uliab build` succeeds.

## Repository layout

```
crates/
  ulb-lang/                  DSL core: lexer, parser, AST, evaluator
  ulb-plugin-sdk/            The plugin ABI (WIT + version constants) — MIT
  ulb-plugin-fixture/        Test plugin (current world), wasm32-wasip2
  ulb-plugin-legacy-fixture/ Test plugin (frozen ABI 0.1 world)
  ulb-plugin-cross-dep-fixture/ Test plugin proving cross-plugin deps
  uliab/                     The CLI, wasmtime host, task engine, resolver
examples/sample-kmp/        Worked example exercising the DSL end to end
docs/                       Engineering documentation (see below)
```

## Building

```sh
rustup target add wasm32-wasip2   # only needed to build plugin fixtures
cargo build --workspace
cargo test --workspace
```

## Quick start

Scaffold a project and build it (JDK on PATH; kotlinc for Kotlin; an
Android SDK for android projects):

```sh
uliab init demo --type jvm      # or --type android / --type kmp
cd demo
uliab build
```

Or drive the pieces individually:

```sh
# Build a plugin fixture to wasm and run it under the host
cargo build -p ulb-plugin-fixture --target wasm32-wasip2
cargo run -q -p uliab -- run \
  target/wasm32-wasip2/debug/ulb_plugin_fixture.wasm 'input'

# Resolve a project's deps {} without building
cargo run -q -p uliab -- deps resolve --project examples/sample-kmp

# List plugins known to the registry
cargo run -q -p uliab -- plugins list
```

`uliab build` runs the full pipeline — evaluate, resolve plugins through
the registry, configure each plugin, execute the task graph incrementally:

```sh
uliab build [--project DIR] [--registry SOURCE] [--cache-dir DIR] [--repo REPO]
```

A build reports per-task `ran` / `up-to-date` lines; a failed task fails
the build with its name and failure payload.

## Plugins

Plugins are `wasm32-wasip2` components exporting the `ulb-plugin` world
(ABI 0.7). They are distributed through a registry index
(`registry/index.json` syntax, see `docs/plugin-registry.md`) and resolved
from `libs.ulb` `plugins {}` tables. Official plugins live in
[`Ulite-Team/ulb-plugins`](https://github.com/Ulite-Team/ulb-plugins):

- `ulite/jvm` — Java/Kotlin compilation, jar packaging, JUnit 4/5 tests,
  KSP2 code generation over Kotlin sources.
- `ulite/android` — resource merge, dexing, per-variant APKs
  (`buildTypes {}` × `productFlavors {}`), APK signing, Compose compiler
  plugin support.
- `ulite/kmp` — `commonMain`/`jvmMain` source sets into a jar with
  per-target JVM tests; an Android target built by composing with
  `ulite/android`.

## Editor tooling

- [`tree-sitter-ulb`](https://github.com/Ulite-Team/tree-sitter-ulb) —
  tree-sitter grammar for presentation (highlighting, folding, indentation).
- [`ulb-lsp`](https://github.com/Ulite-Team/ulb-lsp) — semantic analysis on
  the same AST the evaluator uses (parse diagnostics, lint-mode
  evaluation, hover, goto-definition).

## Documentation

The full engineering documentation lives in [`docs/`](docs/index.md):
the [architecture](docs/architecture.md) and [language spec](docs/grammar.md),
plus references for the [plugin ABI](docs/abi.md),
[host](docs/host.md), [task engine](docs/task-engine.md),
[Maven resolver](docs/maven-resolver.md),
[registry](docs/plugin-registry.md), [CLI](docs/cli.md),
[crates](docs/crates.md), [plugin authoring](docs/authoring-plugins.md),
and [testing](docs/testing.md).

## License

GPL-3.0, except `crates/ulb-plugin-sdk`, which is MIT. See `LICENSE` and
`crates/ulb-plugin-sdk/LICENSE`.
