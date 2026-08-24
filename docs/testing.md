# Testing

The workspace is tested at three levels: library unit tests, a
worked-example integration test, and end-to-end host tests that build
real wasm fixtures.

## Level 1 — `ulb-lang` unit tests

Lexer, parser, AST, evaluator, spans, and diagnostics are covered inline.
The required diagnostic cases from `grammar.md` §11 (unexpected token,
missing value after path, dotted callee, unterminated string / block /
interpolation, reserved word as identifier, misplaced `@`) each have a
test, as do the evaluator's merge rules, interpolation, and hermetic
lint-mode entry points (`evaluate_build_lint` never consults the process
environment or filesystem).

## Level 2 — the worked example

`crates/ulb-lang/tests/worked_example.rs` evaluates
`examples/sample-kmp` end to end with an injected environment
(`STORE_PASSWORD` / `KEY_PASSWORD`) and asserts the resolved module model:
merged conventions, `@`-resolved catalog aliases, repeated pairs
accumulating into lists (including the module's `plugin` statements),
dotted source-set nesting, `ver(...)`, and a
`task` whose `run {}` actions were captured as data rather than executed.
A second test evaluates the example's `settings.ulb` into a clean
`SettingsModel`.

## Level 3 — host integration tests

Three integration test files in `crates/uliab/tests`:

- `build_driver.rs` — drives `build_project` end to end against a temp
  project resolved through a one-entry local registry index: tasks run on
  the first build, are skipped on an unchanged rebuild, and rerun when a
  read file changes.
- `configure_execute.rs` — drives `PluginHost` + the task executor
  against both fixture crates (`ulb-plugin-fixture`, current world;
  `ulb-plugin-legacy-fixture`, legacy world), proving the configure →
  execute path and the frozen-legacy-world loading rule.
- `deps_resolve.rs` — exercises `resolve_project_deps` and the Maven
  resolver against offline local-POM repositories (`LocalRepo`),
  asserting compile/runtime bucketing, scope visibility, and
  highest-version-wins conflict resolution.

## The wasm fixture build flow

The host tests build real `wasm32-wasip2` components before running them.
Shared helpers live at the top of the test files:

- `workspace_root()` — two parents above `CARGO_MANIFEST_DIR` (the repo root).
- `target_dir()` — honors `CARGO_TARGET_DIR`.
- `build_fixture(package)` — `cargo build -p <pkg> --target wasm32-wasip2`,
  returning the artifact at
  `target/wasm32-wasip2/debug/<dashes-as-underscores>.wasm`.
- `LocalRepo` — generates local POMs for offline Maven resolution.

These tests require the `wasm32-wasip2` target installed:
`rustup target add wasm32-wasip2`.

## Running

```sh
cargo test --workspace      # unit + doc + integration
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
```

Doc-tests live inside the source files (the evaluator's entry points and
the diagnostic renderer carry working examples). The `ulb-plugin-fixture`
crate's own unit tests cover config parsing and task registration.
