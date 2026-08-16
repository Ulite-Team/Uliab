# Crates

Single Cargo workspace (`resolver = "2"`, edition 2024, workspace version
`0.1.0`). The workspace lint set denies `unsafe_code`; the only allowance
lives on wasmtime's `bindgen!` / wit-bindgen's `generate!` generated
modules, which use `unsafe` internally and carry a scoped
`#[allow(unsafe_code)]`.

## Members

| Crate | Kind | Job |
|---|---|---|
| `crates/ulb-lang` | library | Lexer, parser, span-annotated AST, and evaluator for the ulb DSL. No wasm, no dependencies on the rest of the workspace. Reused by the `uliab` CLI, `ulb-lsp`, and the worked-example tests. |
| `crates/ulb-plugin-sdk` | library | The plugin ABI as a single source of truth: the `plugin.wit`/`legacy-plugin.wit` texts, the `ABI_VERSION` constant, and the manifest record. MIT-licensed; the rest of the workspace is GPL-3.0. |
| `crates/ulb-plugin-fixture` | cdylib | Test plugin (`ulite/fixture`, ABI 0.5) exercising the `configure` path: parses a module config, registers `stage`/`announce`/`copy-classpath` tasks. Built for `wasm32-wasip2` by the integration tests. |
| `crates/ulb-plugin-legacy-fixture` | cdylib | Test plugin (`ulite/legacy-fixture`) built against the frozen pre-`configure` world (`legacy-plugin.wit`). Proves additive ABI growth keeps old components loadable. |
| `crates/uliab` | binary + library | The build tool itself: CLI, embedded wasmtime host, registry client, Maven resolver, task engine, and the `build_project` driver. |

## Dependency graph

```
ulb-lang  ──► (none)
ulb-plugin-sdk ──► (none)
uliab ──► ulb-lang, ulb-plugin-sdk
ulb-plugin-fixture ──► ulb-plugin-sdk
ulb-plugin-legacy-fixture ──► ulb-plugin-sdk
```

The SDK deliberately has no dependencies: both the host
(`wasmtime::component::bindgen!`) and every plugin
(`wit_bindgen::generate!`) bind from the same WIT text, so the interface
cannot drift between them.

## Crate responsibilities in one sentence each

- **ulb-lang** parses and evaluates `.ulb` files into a generic, JSON-serializable
  module model that plugins consume.
- **ulb-plugin-sdk** owns the interface that separates the host from every plugin.
- **uliab** turns a project directory plus a set of plugins into an executed,
  incrementally-cached task graph.

## Build notes

- Plugin crates (`ulb-plugin-fixture`, `ulb-plugin-legacy-fixture`) and any
  third-party plugin are `cdylib`s compiled to `wasm32-wasip2` and wrapped
  into components. The host crate itself builds for any host target.
- Adding a `wasm32-wasip2` target: `rustup target add wasm32-wasip2`.
- Everything else: `cargo build --workspace` and `cargo test --workspace`
  from the repository root.
