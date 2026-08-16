# Build pipeline (`crates/uliab/src/driver.rs`)

`build_project(dir, options)` is the whole build in one call. The
11-step shape is specified in `architecture.md` §9; this is how the
implementation maps onto it.

## Options

```
BuildOptions {
    registry: Option<RegistrySource>,
    cache_dir: Option<PathBuf>,
    repos:    Option<Vec<MavenRepo>>,
}
```

The three optional knobs override, respectively: which registry index to
resolve plugins against, where plugin artifacts cache, and which Maven
repositories to use (from the `--repo` flag).

## Two phases

The build is split into a **configure phase** and an **execution phase**.
The configure phase produces a validated task graph; nothing runs until
the graph is complete and validated. This is the guarantee `abi.md`
describes: plugins can only register tasks, and only while being
configured.

### Configure phase

1. Read the project's `libs.ulb` via `read_libs_plugins`
   (`crates/uliab/src/project.rs`), lifting each `plugins {}` entry into a
   `PluginSpec { name, version }`.
2. Evaluate the module model (`settings` / `libs` / `conventions` / each
   `build.ulb`) through `ulb-lang`'s evaluator.
3. Resolve every `PluginSpec` through the registry
   ([plugin-registry.md](plugin-registry.md)) into a downloaded, verified
   wasm artifact.
4. Load each plugin and call its `configure` with the JSON serialization
   of the whole module model. The serialization is deterministic:
   `BTreeMap` iteration keeps the JSON byte-stable, so the config
   fingerprint is stable across runs. Scalars / versions / coordinates map
   to strings, blocks to objects, lists to arrays.
5. Resolve the module's `deps {}` through the Maven resolver
   ([maven-resolver.md](maven-resolver.md)); the resolved classpath is
   injected into every plugin's configure JSON (the `classpath.*` keys the
   jvm plugin reads).
6. Each plugin returns a `TaskGraph`; the driver merges them under
   plugin-name task identities, re-validates the merged graph (undefined
   `depends_on`, cycles), and fingerprints the configuration.

### Execution phase

7. Execute the merged task graph incrementally over
   `<project>/.uliab/state.json` — see [task-engine.md](task-engine.md)
   for the UP-TO-DATE rules.
8. Persist the recorded fingerprints.

## What the host injects

Every plugin's `configure` JSON contains at minimum:

- `projectDir` — the project directory the build was started for, so
  plugins resolve their relative paths against the project, not the
  invocation directory;
- the module model: the `jvm {}`/`android {}`-style blocks, `deps {}`,
  properties, and every other evaluated value;
- `classpath.*` buckets for the jvm family, when the module declares deps.

`resolve_project_deps(dir, options)` is the same configure phase without
the execution phase: it resolves and prints the classpath buckets, which
is what `uliab deps resolve` surfaces.
