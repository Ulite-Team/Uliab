# uliab

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

This is early, active development. The target-agnostic redesign is in
place; `ulite/jvm` is the reference plugin; `ulite/android` and
`ulite/kmp` are the roadmap.

## Repository layout

```
crates/
  ulb-lang/                 DSL core: lexer, parser, AST, evaluator
  ulb-plugin-sdk/           The plugin ABI (WIT + version constants) — MIT
  ulb-plugin-fixture/       Test plugin (current world), wasm32-wasip2
  ulb-plugin-legacy-fixture/Test plugin (frozen ABI 0.1 world)
  uliab/                    The CLI, wasmtime host, task engine, resolver
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

```sh
# Build a plugin fixture to wasm and run it under the host
cargo build -p ulb-plugin-fixture --target wasm32-wasip2
cargo run -q -p uliab -- run \
  target/wasm32-wasip2/debug/ulb_plugin_fixture.wasm 'input'

# Resolve a project's deps {} without building
cargo run -q -p uliab -- deps resolve --project examples/sample-kmp
```

`uliab build` runs the full pipeline — evaluate, resolve plugins through
the registry, configure each plugin, execute the task graph incrementally:

```sh
uliab build [--project DIR] [--registry SOURCE] [--cache-dir DIR] [--repo REPO]
```

A build reports per-task `ran` / `up-to-date` lines; a failed task fails
the build with its name and failure payload.

## Plugins

Plugins are `wasm32-wasip2` components exporting the `ulb-plugin` world.
They are distributed through a registry index (`registry/index.json`
syntax, see `docs/plugin-registry.md`) and resolved from `libs.ulb`
`plugins {}` tables. Official plugins live in
[`Ulite-Team/ulb-plugins`](https://github.com/Ulite-Team/ulb-plugins).

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
[Maven resolver](docs/maven-resolver.md), and
[build pipeline](docs/build-pipeline.md).

## License

GPL-3.0, except `crates/ulb-plugin-sdk`, which is MIT. See `LICENSE` and
`crates/ulb-plugin-sdk/LICENSE`.
