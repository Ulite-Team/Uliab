# ulb — Architecture (architecture.md)

System design for the ulb build tool. grammar.md is the language spec;
this document is how the pieces fit together.

**Status:** the tool core is target-agnostic. Java, Kotlin/JVM, Android,
and Kotlin Multiplatform support are not built into the tool; they are
official **plugins**, written in Rust, compiled once to WebAssembly, and
distributed through Ulite Team's own plugin registry (never Gradle's,
never Maven Central for plugin code). This is the single biggest
architectural decision in the project so far — §3 explains why and how.

**Implementation status:** `ulb-lang`, the `uliab` CLI (plugin host,
task engine, Maven resolver, registry client), the tree-sitter grammar,
the LSP, the `ulite/jvm` plugin (Java + Kotlin/JVM incl. KSP + JUnit),
the `ulite/android` plugin (compile + full APK packaging chain), the
`ulite/kmp` plugin (JVM target: commonMain + jvmMain → jar), and
multi-module `settings.ulb` support are implemented and building. The
plugin registry is live on GitHub. Remaining roadmap items: module
dependency syntax, APK signing, build variants, `uliab init`,
KMP Android/native targets.

---

## 1. Goals and non-goals

### Goals

1. Replace Gradle for the author's own projects — plain Java, Kotlin/JVM,
   Android, and Kotlin Multiplatform — using **one DSL and one core
   engine** for all of them, with `if`/`else`/`env()` as native control
   flow, not simulated Kotlin execution.
2. Keep the core tool small, generic, and rarely-changing. Everything
   that churns with the underlying ecosystems (a new AGP quirk, a new
   Kotlin compiler flag, a new Android API level) lives in a plugin that
   ships and versions independently — the core tool should almost never
   need a release just because Android changed something.
3. Ship a real task/variant/dependency engine: parallel DAG execution,
   content-hash fingerprinting, and (where a plugin declares them)
   variant propagation and classpath rules — engine-level concerns the
   core provides as *services* plugins call into, not concerns the core
   hardcodes per target.
4. Editor tooling as a day-one requirement: a tree-sitter grammar for
   syntax presentation and an LSP built on the same typed AST the
   evaluator uses, so semantic diagnostics always match real evaluation
   behavior.

### Non-goals (this pass)

- No Gradle/`.kts` compatibility layer, no Kotlin-scripting bridge, no
  `kotlinc`-as-a-DSL-interpreter anywhere (kotlinc/javac/d8/aapt2 are
  still invoked as external compiler *tools* by plugins — see §3.5 — just
  never to interpret build configuration).
- No third-party (non-Ulite-Team) plugin ecosystem yet — the plugin
  *mechanism* is generic and could load anyone's plugin, but the plugin
  *registry* only carries Ulite Team's own official plugins for now.
- No general "any Android project" compatibility.
- No remote/shared build cache (design leaves room for one).
- No publishing (no maven-publish equivalent).
- No LSP rename / find-all-references / rich semantic tokens yet.

---

## 2. Repositories and crates

Four repositories under `Ulite-Team`:

| Repo | Contains | Role |
|---|---|---|
| `Uliab` | `ulb-lang` crate, `uliab` CLI, plugin host + task engine, `docs/*` (grammar, architecture, ABI, …) | target-agnostic core |
| `tree-sitter-ulb` | `grammar.js`, `highlights.scm`, `folds.scm`, `indents.scm` | editor syntax presentation |
| `ulb-lsp` | `ulb-lsp` binary | LSP server |
| `ulb-plugins` | `jvm`, `android`, `kmp` plugin crates + the plugin registry index | official target plugins |

```
┌─────────────────────────────── Uliab ────────────────────────────────┐
│  ulb-lang  (lib crate — unchanged by this redesign)                  │
│  ┌────────┐  ┌────────┐  ┌─────────┐  ┌────────────┐                │
│  │ lexer  │→ │ parser │→ │   AST   │→ │ evaluator  │ → generic Value │
│  └────────┘  └────────┘  └─────────┘  └────────────┘   module model  │
│                                                                       │
│  uliab CLI: plugin host + task engine + resolver + fingerprinting    │
│  — has ZERO hardcoded knowledge of "android", "compileSdk", APKs,    │
│    dex, manifests, or KSP. All of that lives in a plugin (§3).       │
└────────────────────────────────────────────────────────────────────┘
    │ path dep         │ loads *.wasm at runtime, sandboxed (§3.4)
    ▼                  ▼
┌─────────────────┐   ┌─────────────────────────────────────────┐
│  ulb-lsp        │   │  ulb-plugins/{jvm,android,kmp}           │
│  reuses parser  │   │  Rust source → compiled once to .wasm    │
└─────────────────┘   │  published to the Ulite Team registry    │
                       └─────────────────────────────────────────┘
┌─────────────────────┐
│  tree-sitter-ulb    │  (depends on grammar.md conceptually only)
│  presentation only  │
└─────────────────────┘
```

**Why this doesn't touch `ulb-lang` at all:** the evaluator already
resolves `android { compileSdk 37 }` into a *generic* nested `Value::Block`
— it never hardcoded a `compileSdk: u32` field anywhere. The DSL's
"contextual identifiers" design (grammar.md §4: `android`, `buildTypes`,
`compileSdk`, … are plain identifiers, not reserved words) means the
language was already plugin-friendly before this redesign existed; this
document is about who gets to *assign meaning* to those keys downstream,
not about reworking the parser/evaluator.

---

## 3. The plugin system

### 3.1 What moves out of the core

Everything in the old §5, §7, §8, §9 (variant matrix, KMP source-set
hierarchy, manifest merging, KSP) and the Android-specific parts of the
old §3.5's `ModuleModel` (an `android`/`buildTypes`/`flavors` field) is
**not core tool knowledge anymore**. The core only ever produces the
generic `Value::Block` the evaluator already builds (`eval.rs`); a plugin
is the thing that looks at `top["android"]`, decides what the keys inside
it mean, and turns them into tasks.

What the core *does* keep, because it's genuinely target-agnostic:

- The DSL itself (lexer/parser/evaluator, unchanged).
- The task DAG engine: topological + parallel scheduling, fingerprinting,
  UP-TO-DATE caching (old §4 — a plugin *registers* tasks into this
  engine; the engine doesn't know or care what a task's `run` action is).
- The Maven dependency resolver (`deps { implementation "..." }` — POM
  fetching, transitive resolution, `api`/`implementation` classpath
  visibility rules) — every JVM-family plugin (jvm/android/kmp) needs the
  same resolver, so it stays core rather than being reimplemented three
  times.
- The plugin host itself (§3.3–§3.5) and the plugin registry client
  (§3.6).

### 3.2 A plugin, concretely

A plugin is a Rust crate compiled to a `.wasm` module (target
`wasm32-wasip2` — see §3.4 for why WASM specifically) that implements a
small, fixed set of host-callable entry points:

```rust
// Illustrative shape of the plugin ABI's Rust-side trait — the concrete
// wasm boundary is the WIT world described after this block, and this is
// what a plugin author writes against via a `ulb-plugin-sdk` crate that
// hides the wasm/FFI plumbing.
pub trait UlbPlugin {
    /// e.g. "android", version "1.4.0" — checked against the tool's
    /// plugin-ABI version (§3.7) before the plugin is loaded at all.
    fn manifest(&self) -> PluginManifest;

    /// Given the module's resolved config block (e.g. `top["android"]`,
    /// `top["buildTypes"]`) and the already-resolved dependency graph,
    /// validate the keys this plugin owns and register tasks + their
    /// inputs/outputs/dependsOn edges into the task DAG. This is the
    /// plugin's only way to affect the build — it cannot run arbitrary
    /// code outside the actions in §3.5.
    fn configure(&self, module: &ModuleConfig, host: &mut TaskRegistrar) -> Result<(), PluginError>;
}
```

The wasm boundary itself is a WIT world in `ulb-plugin-sdk` (see
`docs/abi.md`): the plugin **exports** `manifest`, `configure`, and `run`
entry points and **imports** a `task-registrar` interface. Across that
boundary the module model travels as a JSON serialization (the whole
resolved `Value` tree plus the resolved classpaths — see §9), not as
typed Rust structs; the trait above is the shape a plugin author writes
against before the SDK compiles it to wasm. `docs/authoring-plugins.md`
shows the end-to-end flow a plugin author follows.

A plugin does **not** get a raw filesystem/network/process handle. It
gets `host: &mut TaskRegistrar` — a narrow, capability-based API for:
registering a task with a set of *declared* inputs/outputs (for
fingerprinting) and a *declared* action (§3.5: run an allowlisted
external tool with an argument list the plugin computes, or a `copy`),
and reading specific already-resolved values out of `ModuleConfig`
(resolved classpaths, resolved deps, the generic config `Value` tree).
This mirrors the DSL's own "closed action set" philosophy (grammar.md
§7) at the plugin layer: a plugin can be *wrong*, but it can't be
*unsafe* in ways the host didn't explicitly allow.

The one ambient capability beyond compute is what the host preopens. Its
WASI context wires stdout/stderr to the build's and, when a build runs
against an Android SDK, preopens that SDK directory **read-only at its
real path** (§3.3 injection: `androidSdkDir`) so a plugin can discover
SDK components itself — platform jars, `build-tools` binaries — instead
of asking the host to embed Android-specific probing logic. A module
that declares its own `android.sdkDir` gets the same capability for that
path (resolved against the project directory when relative): the host
stays Android-agnostic and merely preopens the directories the module
names. Everything else outside the plugin's own wasm memory stays
unreachable: the guest filesystem is empty unless an SDK root is
configured.

### 3.3 How a plugin gets applied

`libs.ulb`'s existing `plugins {}` syntax (grammar.md §6.4 — this needed
**no grammar change**, only a semantic reinterpretation) now names an
Ulite Team plugin, not a Maven/AGP coordinate:

```
plugins {
  android = "ulite/android" @ "1.4.0"
  kotlinJvm = "ulite/kotlin-jvm" @ "2.1.0"
}
```

`build.ulb`'s existing `plugin "alias"` pair statement (grammar.md
Appendix C) applies one:

```
plugin "android"
```

At evaluation time this is still just a `Pair` in the generic `Value`
tree, exactly as before (`top["plugin"] = Value::Str("android")` or a
`Value::List` if repeated — see `eval.rs`'s accumulate rule). The *tool*
(not the evaluator) reads that list after evaluation, resolves each name
against `libs.ulb`'s `plugins {}` table to get a registry coordinate +
version, and loads the corresponding plugin (§3.6) before building the
task DAG.

In the current driver, plugin selection is driven entirely by `libs.ulb`'s
`plugins {}` table: the driver reads it, resolves each entry through the
registry, and applies every resolved plugin to the module. A
`plugin "alias"` statement in `build.ulb` is grammar-legal (it evaluates
to data) but is not yet consumed by the driver; wiring it to select or
gate which registered plugins apply is future work.

### 3.4 Why WebAssembly, not a native `.so`/`.dylib`

This was the open design question worth being explicit about (flagged
for confirmation, not a silent pick):

- **No ABI drift.** Rust has no stable ABI across compiler versions —a
  plugin compiled with one `rustc` and a tool built with a different one
  is undefined behavior if loaded as a native `dylib` via `extern "C"`.
  WASM's binary format is stable regardless of which Rust version
  produced it; a plugin published once keeps working as the core tool's
  own toolchain moves forward.
- **"Not compiled, understood by the tool" is literal, not aspirational.**
  The plugin author compiles once (`cargo build --target wasm32-wasip2`)
  when they publish it. Every user of the tool just *runs* that
  already-compiled artifact through an embedded WASM runtime
  (`wasmtime`) — no local Rust toolchain, no per-user compilation step,
  near-native execution speed (WASM is a compiled bytecode format, not
  an interpreted scripting language — this is why plugin execution is
  "Rust speed", genuinely, not just Rust-flavored).
- **Sandboxing for free.** A `.wasm` module has no ambient filesystem,
  network, or process access unless the host explicitly grants it via
  WASI capabilities. This is what makes the capability-based
  `TaskRegistrar` API (§3.2) enforceable rather than just a convention a
  well-behaved plugin follows — a misbehaving or compromised plugin
  physically cannot reach outside what it was granted.
- **Cross-platform for free.** The same `.wasm` artifact runs unmodified
  on any development or CI machine — no per-platform plugin builds to
  publish or cache.

Trade-off worth naming honestly: calling into WASM has real (small)
overhead versus a native `dylib` call, and a plugin cannot itself spawn
arbitrary subprocesses — every external tool invocation (`kotlinc`,
`javac`, `d8`, `aapt2`) has to go through a host-provided "run this
allowlisted tool with these arguments" capability (§3.5), which is
slightly more ceremony for plugin authors than `std::process::Command`
would be. Both trade-offs are accepted as the right price for ABI
stability and sandboxing.

### 3.5 External toolchain invocation

Plugins still need `kotlinc`, `javac`, `d8`, `aapt2`, KSP, etc. — these
are not reimplemented in Rust/WASM. The host exposes exactly one relevant
capability:

```rust
fn run_tool(&mut self, tool: AllowlistedTool, args: Vec<String>, cwd: &Path)
    -> Result<ToolOutput, PluginError>;
```

`AllowlistedTool` is a closed enum the *core* defines — currently `cp`,
`cat`, `mkdir`, `echo` for filesystem plumbing, `javac`, `kotlinc`,
`jar`, `java` for the JVM toolchain, `aapt2` and `apksigner` (both
resolved under the Android SDK `build-tools` directory a task names as
its first argument) for asset packaging and APK signing (see
`docs/abi.md` for the exact set) — and a plugin
manifest must declare which tools it needs (checked at load time, so a
plugin can't silently start invoking something new after being
installed). The plugin computes *what* to run (e.g. every `kotlinc` flag
for a given module + variant); the host actually spawns
the process. This is the same "plugin decides, host executes" split as
`TaskRegistrar` (§3.2) and mirrors the DSL's own `copy`/`exec` design
(grammar.md Appendix C) one layer up.

### 3.6 Plugin registry & resolution

- A plugin coordinate (`"ulite/jvm"` in the example above) resolves
  against `registry/index.json` in the public `ulb-plugins` repository,
  mirroring how `libs.ulb`'s Maven coordinates resolve against Maven
  repos, but *never touching* Maven Central, Google Maven, or the Gradle
  Plugin Portal. The client's default registry source is the index's raw
  GitHub URL; a different source can be passed with `--plugin-registry`.
- **Hosting.** Plugin artifacts are published to GitHub releases
  (`hello-plugin-v0.4.0`, `jvm-plugin-v0.5.0`), with the `.wasm` binaries
  as release assets that each index entry's `artifact_url` points at.
- **Index format.** `index.json` is a single document with a
  `schema_version` and a `plugins` table keyed by plugin name; each entry
  maps version strings to an ABI range and an artifact URL:

  ```json
  {
    "schema_version": 1,
    "plugins": {
      "ulite/hello": {
        "versions": {
          "0.1.0": {
            "abi": { "min": "0.1", "max": "0.1" },
            "artifact_url": "…/hello_plugin.wasm"
          }
        }
      }
    }
  }
  ```

  `abi` is the plugin-ABI range that build declares support for
  (§3.7) — the value the tool's compatibility check keys on.
  `artifact_url` is an HTTP(S) URL or a `file://`/relative filesystem path
  (the local forms exist so the client can be tested and run without a
  network). Before an artifact is cached, the tool instantiates it and
  cross-checks its `manifest` entry against the index row: name and
  version must match the coordinate and the reported ABI version must lie
  inside the declared range.
- Local plugin cache: `~/.cache/uliab/plugins/<name>/<version>/plugin.wasm`
  plus an `abi.json` recording the ABI range the cached build was verified
  under — checked first; the tool only fetches from the registry on a
  cache miss. A cached build whose recorded range no longer contains the
  host ABI (the tool was upgraded) is refetched. This directly parallels
  the Maven artifact cache design but is a fully separate cache root —
  plugin artifacts and Maven artifacts are never comingled.
- **Version compatibility**: each plugin declares which core plugin-ABI
  version range it targets; the host refuses to load a plugin whose range
  does not contain the host ABI, checked before the plugin is
  instantiated. Keeping the last-known-compatible build running when the
  host upgrades past a plugin's declared range is designed but not
  implemented — today that situation is a hard error. A plugin update is
  opt-in, never forced by a core tool update.

### 3.7 Plugin-ABI versioning

The `UlbPlugin` trait (§3.2) and the `TaskRegistrar`/`run_tool`
capability surface (§3.2, §3.5) together are "the plugin ABI." It is
versioned independently of the core tool's own version (semver, with the
compatibility behavior from §3.6). Growing the ABI (a new
capability, a new field on `ModuleConfig`) is additive-only within a
major version; anything else is a major-version bump, which is exactly
the kind of change that should be rare precisely because plugins — not
the core — absorb almost all day-to-day churn (§1 goal 2).

---

## 4. Task graph & execution engine

Unchanged from before this redesign, except tasks are now always
*plugin-registered* rather than ever core-hardcoded — `clean`/`test`
etc. are conventions plugins are expected to register under, not special
cases the engine knows about.

### 4.1 Model

- **Task** = name, module, inputs (file set + content-hash fingerprints),
  outputs (file set), dependencies (edges), a run action from the closed
  set the engine understands: `copy`, `write` (§4.1 action list),
  `run_tool` (§3.5) with an allowlisted tool + args. The `write` action
  emits a file with fixed contents (creating parent directories) and is
  how a plugin synthesizes generated sources, such as the jvm plugin's
  test runner.
- The **task graph** is the transitive closure of `dependsOn` edges
  across all modules and all plugins active on those modules. Edges are
  directed, acyclic (cycle = resolution error with the cycle path in the
  message).

### 4.2 Execution model (topo + parallel + fingerprinting)

1. **Topological order** over the full task DAG.
2. **Parallelism:** independent branches (no dependency relationship) run
   concurrently, bounded by a worker pool sized to `nproc`. The DAG is
   partitioned into waves; within a wave, task order is stable
   (declaration order) so builds are reproducible.
3. **UP-TO-DATE:** a task is skipped iff every input fingerprint matches
   the recorded fingerprints of the last successful run (§10 below) and
   all of its dependencies ran UP-TO-DATE.
4. **Failure semantics:** a failing task marks the build failed;
   dependents are not started; already-scheduled independent tasks
   continue to completion. No partial-success reporting.

---

## 5. What the official plugins each own

This section replaces the old hardcoded §5/§7/§8/§9 — it now describes
what `ulb-plugins/jvm`, `ulb-plugins/android`, and `ulb-plugins/kmp` are
each *responsible for designing and shipping*, not core-tool behavior.
Each plugin publishes its own reference doc (mirroring grammar.md
Appendix A's tables, but plugin-owned) describing exactly which keys it
understands inside the blocks it claims.

### 5.1 `ulite/jvm` — plain Java & Kotlin/JVM

- Owns: compiling `.java`/`.kt` sources with `javac`/`kotlinc` (via
  `run_tool`, §3.5), packaging a `.jar`, registering `compile`/`test`/
  `assemble` tasks.
- Owns the `api`/`implementation`/`runtimeOnly`/`compileOnly`/`ksp`/
  `testImplementation` dependency-scope *semantics* on top of the core
  resolver's already-generic `deps {}` parsing (grammar.md Appendix B —
  the DSL syntax for these scopes is core/shared; what each scope *means*
  for a classpath is jvm-plugin behavior, reused by android/kmp below).
- Owns KSP invocation (generate → compile → package ordering) — KSP is a
  Kotlin-ecosystem concern, not an Android-specific one, so it lives here
  and `ulite/android` depends on `ulite/jvm` to get it rather than
  reimplementing it.

**Implemented:** `ulite/jvm` 0.5.0 builds plain Java and Kotlin/JVM
modules end to end — `compile`/`assemble`/`test` tasks registered into
the core task engine, Maven resolution of `deps {}` scopes, KSP wired
through `kotlinc`, a test task that runs JUnit Platform tests via the
host's `java` tool, and a `jar` packaging step. The host's `write` action
(§4.1) is how the plugin materializes its generated test runner.

### 5.2 `ulite/android` — depends on `ulite/jvm`

- Owns everything under `android {}`, `buildTypes {}`, `productFlavors
  {}`, `signing {}` (the old grammar.md Appendix A tables move to this
  plugin's own docs — grammar.md itself no longer claims these keys as
  core language, see the note at the top of that document's Appendix A).
- Owns the variant matrix (build type × flavor dimensions), per-variant
  source-set layering, and down-module variant propagation — the old §5
  content, now android-plugin-owned logic that calls into the *core*
  task engine (§4) the same way any other plugin does.
- Owns manifest merging (old §8) and dex/APK/AAR packaging via `aapt2`/
  `d8` (`run_tool`).
- Declares a dependency on `ulite/jvm` in its own plugin manifest (§3.2)
  so the tool loads both and `ulite/android` can call into `ulite/jvm`'s
  registered compile tasks rather than re-implementing compilation.

Cross-plugin composition is designed on paper only: no plugin-to-plugin
dependency mechanism exists in the ABI today. What exists so far is the
compile and variant slice (`ulb-plugins/android-plugin`,
`docs/android-plugin.md`): an `android {}` block with `compileSdk`,
`sources`, `namespace`, manifest, and resource directory; toolchain
discovery of the platform jar and the highest `build-tools` release
carrying `aapt2`/`d8` (both the resolved root and a module-declared
`sdkDir` are preopened read-only, §3.2); the variant matrix (build types
x product flavors, or the default `[debug, release]` pair) with
per-variant `linkResources`/`compile`/`d8`/`package`/`sign` tasks;
flavor-level `minSdk` override and `applicationIdSuffix` passed via
`--rename-manifest-package` to `aapt2 link`; APK signing via `apksigner`
when the module's `signing {}` block is present (passwords written to
temp files and passed via `--ks-pass file:`/`--key-pass file:`).
Manifest merging and additional packaging features remain future slices
of the same plugin.

### 5.3 `ulite/kmp` — depends on `ulite/jvm`, optionally `ulite/android`

- Owns the KMP source-set hierarchy (old §7): `commonMain` →
  `androidMain`/`iosMain`/`desktopMain`/…, default hierarchy matching
  Kotlin's published defaults or an explicit one declared in `build.ulb`.
- Owns per-source-set `deps {}` scoping (`commonMain.deps { }` — DSL
  syntax already exists per grammar.md §6.4; resolving which deps are
  visible to which platform source set is `ulite/kmp` behavior). The host
  already resolves every nested `deps {}` block into per-source-set
  classpaths and injects them as `classpathSourceSets` (§9 step 7), so the
  plugin's job is the *hierarchy*: which source sets feed which target.
- For a KMP module's Android target specifically, delegates to
  `ulite/android`; for other native targets (iOS, desktop), delegates to
  whatever future plugins own those toolchains (not designed yet —
  explicitly out of scope this pass, same as the old document's stance).

Not started.

---

## 6. Multi-module dependency graph (core)

Unchanged and still core, because every plugin needs it:

### 6.1 Graph model

`settings.ulb` declares the module list; each `build.ulb`'s `deps {}`
block declares project-module dependencies. The driver evaluates all
modules in a first pass, discovers each module's output artifact
(`jvm.jarFile` or `android.apk`), then resolves `project(":mod")` refs
in a second pass before configuring plugins. The resolver skips
`ProjectRef` entries during Maven resolution; `extract_project_deps`
collects them for the host, and `resolve_project_classpath` maps them to
jar paths on the classpath. The `api`/`implementation` distinction
carries through: both inject the referenced module's jar into compile and
runtime classpaths, while `runtimeOnly` injects into runtime only.

### 6.2 `api` vs `implementation` classpath rules (core resolver, jvm-family-wide)

- **`implementation`** dependencies of a module appear on that module's
  compile/runtime classpath but not on consumers' compile classpath.
- **`api`** dependencies are transitively visible on consumers' compile
  classpath too.
- Compile classpath = module deps (`api`+`implementation`) + transitively
  all `api` edges of those deps. Runtime classpath = transitive closure
  of `api`+`implementation` (conflict resolution: highest version wins,
  §7.3 below).
- This lives in the *core* resolver (not a plugin) because `ulite/jvm`,
  `ulite/android`, and `ulite/kmp` all need identical classpath-visibility
  semantics — duplicating it per plugin would be exactly the kind of
  churn-multiplier this redesign exists to avoid.

### 6.3 Parallel independent branches

Module subtrees with no dependency relationship are scheduled in
parallel by the task engine (§4.2).

---

## 7. Dependency resolution, repositories & cache (core)

### 7.1 Maven repositories (unchanged)

- Built-in defaults: Google Maven then Maven Central.
- Additional repositories are passed on the command line today (`--repo`,
  repeatable), tried in declared order before falling back to defaults.
  Declaring them inside the project — `settings.ulb`'s
  `repositories { maven "url" }` — is not wired up yet: the evaluator
  treats the block as data, but the resolver currently has no settings
  source to read it from.
- This is entirely separate from **plugin** resolution (§3.6), which
  never touches Maven — two independent coordinate spaces, two
  independent caches, on purpose (a compromised or malformed Maven repo
  should never be able to serve something the tool loads as a plugin).

### 7.2 Maven artifact cache layout

- Default: `~/.cache/uliab/modules/<group>/<artifact>/<version>`,
  content-addressed, independent of Gradle's layout.
- The designed `lspCompat true` option (additionally hardlinking resolved
  artifacts into Gradle's cache path for LSPs that expect it, e.g.
  `kmp-lsp`) is not implemented — deferred.

### 7.3 Conflict resolution

Same `group:artifact` at different versions → highest version wins
(declared constraint order on tiebreak); conflicts are recorded as info
diagnostics, never silently downgraded.

---

## 8. Editor tooling: tree-sitter vs LSP

Unchanged by this redesign — plugins affect what the *evaluator's
output* means, not the syntax grammar or the LSP's use of the AST.

| Concern | Owner |
|---|---|
| syntax highlighting, folding, indentation, structural editing | `tree-sitter-ulb` |
| parse + semantic diagnostics, hover, goto-definition, completion | `ulb-lsp` |

The LSP does **not** use tree-sitter for analysis; it walks the same
typed AST + spans from `ulb-lang` the evaluator uses. One consequence of
this redesign worth flagging: some semantic diagnostics (e.g. "unknown
key inside `android {}`") now depend on which plugin's manifest is
active for a given `build.ulb`, not on a fixed core table — `ulb-lsp`
will need to load the same plugin manifests (not the full WASM
execution, just the declared-keys metadata from §3.2's `manifest()`) to
offer that specific diagnostic. Not designed in detail yet; noted here so
it isn't lost.

### 8.1 Grammar sync-by-hand risk (unchanged)

`tree-sitter-ulb/grammar.js` is maintained by hand against
`Uliab/docs/grammar.md`. Mitigations unchanged from before: grammar.md is
written for mechanical portability (grammar.md §8); a grammar change in
one repo produces a `PROGRESS.md` entry in the others; the
`tree-sitter-ulb` test suite parses the same example files as
`ulb-lang`'s snapshot tests.

---

## 9. End-to-end build pipeline

```
1. Parse+eval settings.ulb           → project name, module list (absent = single-module)
2. Parse+eval libs.ulb               → version catalog + plugin coordinates
3. Parse+eval conventions.ulb        → convention + fn tables
4. For each module directory (multi) or single module:
   a. Parse+eval build.ulb           → generic Value module model
   b. Discover module output          (jvm.jarFile / android.apk) — multi only
5. Resolve every entry of libs.ulb's `plugins {}` table against the registry
6. Load each resolved plugin (cache hit, or fetch from the registry — §3.6)
7. Each plugin's configure() validates its owned keys in the module model
   and registers tasks into the core task engine (§3.2, §4)
8. Resolve external Maven deps against repos → cache (§7) — core, shared
   by every plugin active on the module. The module's top-level `deps {}`
   block resolves to the `classpath` configuration key; every nested
   `deps {}` block — `commonMain.deps`, `androidMain.deps`, or deeper such
   as `kmp.commonMain.deps` — resolves independently and is injected as
   `classpathSourceSets`, mapping each source-set path to its own classpath
   (§5.3). Both are part of the configuration hash (§10)
9. Resolve `project(":mod")` refs against discovered outputs; merge the
   referenced module's jar into the depending module's classpath (multi only)
10. Derive the full task DAG (§4); fingerprint inputs; schedule waves
11. Execute: each task's action runs via `run_tool`/`copy`/`write` (§3.5)
    inside a sandboxed working directory
12. Record fingerprints for the next build
```

Steps 1–9 are the "configuration phase" (deterministic, side-effect-free
except cache reads/writes and plugin-loading I/O). Steps 10–12 are the
"execution phase." A project without `settings.ulb` follows steps 2–12
over a single module (backward-compatible). This is the same two-phase
shape as before the redesign; what changed is that steps 4–9 didn't
exist as *core* responsibilities before (they were folded into hardcoded
Android logic).

---

## 10. Fingerprinting & UP-TO-DATE caching (core)

- Every `.ulb` file is content-hashed (SHA-256) at configuration time.
- A task's **input fingerprint** covers: every `.ulb` file it depends on
  (project files, conventions, catalog), the resolved plugin versions
  (`name@version`), the resolved dependency classpath for the module, the
  task's declared input files (a missing file hashes as absent), and the
  rendered action string (tool + arguments). A change to any of them
  invalidates the task. Content-addressed, not timestamp-based.
- A task is UP-TO-DATE when its current input fingerprint equals the
  recorded one and all dependencies are UP-TO-DATE (§4.2).
- Fingerprints are persisted in the project's `.uliab/state.json`
  (versioned format). Output files are *not* fingerprinted — a task
  whose outputs were deleted externally is not re-run. Known limitation,
  tracked.
- No remote cache this pass; the fingerprint format should not preclude
  one later.

---

## 11. Crate/phase roadmap

| Phase | Scope | Deliverable |
|---|---|---|
| 1 (done) | grammar.md + architecture.md | design lock, review |
| 2 (done) | `ulb-lang` workspace + lexer + AST | tokens, spans, one test per construct |
| 3 (done) | `ulb-lang` parser + error recovery | partial AST + diagnostics, snapshot + malformed tests |
| 4 (done) | `ulb-lang` evaluator + worked example | generic `Value` model, conventions/fn/env, end-to-end example |
| 4.5 (done) | grammar.md's Android/KMP keys reframed as plugin-owned | grammar.md edit, no code |
| 5 (done) | `tree-sitter-ulb` | grammar.js + highlights/folds/indents |
| 6 (done) | `ulb-lsp` | didChange parse diagnostics + semantic diagnostics on the shared AST |
| 7a (done) | `uliab` CLI: plugin host + WASM runtime embedding + registry client (§3) | loads and calls a trivial "hello world" plugin end-to-end |
| 7b (done) | Core task engine (§4) + fingerprinting (§10), target-agnostic | schedules/executes a task graph a test plugin registers |
| 7c (done) | Core Maven resolver + classpath rules (§6, §7) | `deps {}` resolves to a real classpath, shared by any plugin |
| 8a (done) | `ulite/jvm` plugin | builds a plain Java or Kotlin/JVM module end-to-end (incl. KSP, tests) |
| 8b (done) | `ulite/android` plugin (depends on 8a) | builds a real Android module end-to-end (compile + APK packaging) |
| 8c (done, jvm slice) | `ulite/kmp` plugin (depends on 8a, 8b) | jvm target: commonMain + jvmMain → jar; Android/native targets deferred |
| 9 (done) | Multi-module `settings.ulb` | project name, module list, per-module build.ulb, extra repos |
| 10 (done) | Module dependency syntax | `implementation project(":shared")` in deps {}, cross-module classpath |
| 11 (done) | APK signing | release/debug keystores, signing {} block, v1/v2/v3 schemes |
| 12 | Build variants | debug/release/flavor splits, per-variant task naming |
| 13 | `uliab init` | scaffold new project from templates |
| 14 | KMP Android target | androidMain source set, plugin-to-plugin composition |

Phases 2–6 are sequential (each depends on the previous); 7a/7b/7c are
independent core services and were built in parallel on top of 4. 8a
landed before 8b/8c (both depend on it); 8b and 8c landed in parallel.
Phase 9 (settings.ulb) landed after 8c. Phase 10 (module dependency
syntax) landed after 9. Phase 11 (APK signing) landed after 10.
Phases 12–14 are the remaining
path to a production-ready build tool: 12 adds build-quality splits,
13 improves developer experience, and 14 completes the KMP story.

---

## 12. Out of scope & deferred (explicit)

From the original spec: Gradle/.kts compat, Kotlin-scripting bridge,
kotlinc-as-DSL-interpreter, third-party (non-Ulite-Team) plugin registry
sources, general Android compat, remote cache, publishing, LSP rename/
find-all-references/advanced semantic tokens.

New from this redesign, explicitly not designed yet:
- Plugin-to-plugin dependency / cross-plugin composition in the ABI
  (§5.2 — needed for KMP Android target, not designed yet).
- `ulb-lsp` loading plugin manifests for plugin-owned diagnostics (§8).
- Non-JVM KMP native targets (iOS, desktop) — deferred until cross-
  compilation toolchain integration is designed.
