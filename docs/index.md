# ulb — Documentation

This directory holds the engineering documentation for the ulb build tool.
It is written from the implementation, not the other way around: every
claim here is either a direct statement about the code in this repository
or a link to one of the two anchor documents.

## Anchor documents

| Document | What it covers |
|---|---|
| [architecture.md](architecture.md) | System design: goals, the plugin system, task engine, dependency resolution, registry, editor tooling split, roadmap |
| [grammar.md](grammar.md) | The ulb language: EBNF, lexical structure, reserved vs contextual words, per-role statement rules, error-recovery contract |

These two are the design documents. They were written before most of the
implementation and are deliberately normative: where the code disagrees,
the code is the truth and the document has drifted (known drift is called
out inline).

## Reference documents

| Document | What it covers |
|---|---|
| [crates.md](crates.md) | Workspace map: each crate, its job, and how the pieces depend on each other |
| [abi.md](abi.md) | The plugin ABI as it actually exists: the WIT world, the `manifest`/`configure`/`task-registrar` contract, and ABI versioning policy |
| [authoring-plugins.md](authoring-plugins.md) | How to write a WASM plugin against the ABI, end to end |
| [host.md](host.md) | The embedded wasmtime host: plugin loading, ABI checks, the registrar |
| [task-engine.md](task-engine.md) | `TaskGraph`, the executor, incremental UP-TO-DATE semantics, fingerprinting |
| [maven-resolver.md](maven-resolver.md) | Maven dependency resolution: repositories, POM parsing, scope buckets, version selection |
| [plugin-registry.md](plugin-registry.md) | The plugin registry client and `index.json` format |
| [build-pipeline.md](build-pipeline.md) | The `build_project` driver pipeline from `libs.ulb` to executed tasks |
| [cli.md](cli.md) | The `uliab` command-line surface |
| [testing.md](testing.md) | How the workspace is tested, including the wasm fixture build flow |

## How to read these

1. Start with `architecture.md` §2 (repositories and crates) and §3 (the
   plugin system) — that is the shape of the whole project.
2. If you are touching the language, read `grammar.md` before `crates/ulb-lang`.
3. If you are writing a plugin, read `abi.md` and `authoring-plugins.md`,
   then look at `examples/sample-kmp` and the `ulb-plugin-fixture` crate.
4. If you are debugging a build, read `build-pipeline.md`, then
   `task-engine.md`.

## Sibling repositories

| Repository | Purpose | Docs |
|---|---|---|
| `Ulite-Team/ulb-lsp` | Language server for the ulb DSL, built on this repo's `ulb-lang` crate | [`/docs`](https://github.com/Ulite-Team/ulb-lsp) |
| `Ulite-Team/tree-sitter-ulb` | tree-sitter grammar for presentation editing | [`/docs`](https://github.com/Ulite-Team/tree-sitter-ulb) |
| `Ulite-Team/ulb-plugins` | Official WASM plugins (`ulite/hello`, `ulite/jvm`) and the plugin registry index | [`/docs`](https://github.com/Ulite-Team/ulb-plugins) |

The split is deliberate: the core stays target-agnostic, all Java/Kotlin
build logic lives in plugins, tree-sitter handles presentation, and the
LSP does semantic analysis on the same AST the evaluator uses.
