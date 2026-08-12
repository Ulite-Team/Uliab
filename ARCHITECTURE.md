# ulb — Architecture (ARCHITECTURE.md)

System design for the ulb build tool, written before deep implementation so
the design can be reviewed against the spec. GRAMMAR.md is the language
spec; this document is how the pieces fit together.

**Status:** draft for review (Phase 1). Written before any parser code.

---

## 1. Goals and non-goals

### Goals

1. Replace Gradle for the author's own multi-module Android / Kotlin
   Multiplatform projects, with a declarative DSL whose control flow
   (`if`/`else`, `env()`) is *native* — not simulated Kotlin execution.
2. Ship a real task/variant/dependency engine under the DSL: parallel DAG
   execution, flavor×buildType variant propagation, `api`/`implementation`
   classpath correctness.
3. Editor tooling as a day-one requirement: a tree-sitter grammar for
   syntax presentation and an LSP built on the *same* typed AST the
   evaluator uses, so semantic diagnostics always match real evaluation
   behavior.

### Non-goals (this pass)

- No Gradle/`.kts` compatibility layer, no Kotlin-scripting bridge, no
  `kotlinc` dependency in the tool itself.
- No third-party plugin ecosystem; only conventions the author writes.
- No general "any Android project" compatibility.
- No remote/shared build cache (design leaves room for one).
- No publishing (no maven-publish equivalent).
- No LSP rename / find-all-references / rich semantic tokens yet.

---

## 2. Repositories and crates

| Repo | Contains | Role |
|---|---|---|
| `Ulite-Team/Uliab` | `ulb-lang` crate, `uliab` CLI (later), `GRAMMAR.md`, `ARCHITECTURE.md` | core |
| `Ulite-Team/tree-sitter-ulb` | `grammar.js`, `highlights.scm`, `folds.scm`, `indents.scm` | editor syntax presentation |
| `Ulite-Team/ulb-lsp` | `ulb-lsp` binary | LSP server |

```
┌─────────────────────────────── Uliab ───────────────────────────────┐
│  ulb-lang  (lib crate)                                               │
│  ┌────────┐  ┌────────┐  ┌─────────┐  ┌────────────┐               │
│  │ lexer  │→ │ parser │→ │   AST   │→ │ evaluator  │ → ModuleModel │
│  └────────┘  └────────┘  └─────────┘  └────────────┘               │
│  (spans, error recovery)        (tree-walker, side-effect-free)     │
│                                                                     │
│  uliab CLI (later phase) — task engine, resolver, build pipeline    │
└─────────────────────────────────────────────────────────────────────┘
         │                                  │
         │ path dep                         │ depends on GRAMMAR.md
         ▼                                  ▼ (read-only reference)
┌─────────────────┐              ┌─────────────────────┐
│  ulb-lsp        │              │  tree-sitter-ulb    │
│  tower-lsp      │              │  grammar.js + scm   │
│  reuses parser  │              │  presentation only  │
└─────────────────┘              └─────────────────────┘
```

Dependency direction: `ulb-lang` is a standalone library with **zero**
CLI/build-tool dependencies. `ulb-lsp` depends on `ulb-lang` via path
during development (published/vendored once versioning exists).
`tree-sitter-ulb` depends on `GRAMMAR.md` conceptually only — it is kept in
sync by hand (see §11).

---

## 3. `ulb-lang` crate

### 3.1 Lexer

- Token stream with `SourceSpan` (byte range) on every token.
- Tokens: `IDENT`, `NUMBER`, `STRING`, `BOOL`, reserved words, symbols
  (`{ } ( ) [ ] = , . @ ! == != < <= > >= && ||`), comments (kept as
  tokens for LSP tokenization, ignored by the parser).
- String lexing tracks `${`...`}` interpolation nesting (GRAMMAR.md §3).

### 3.2 Parser

- Hand-written recursive descent; no parser-generator dependency.
- Dispatches per GRAMMAR.md §5.1 — every choice decided by current token
  or one-token lookahead.
- **Never fail-fast:** produces a partial AST plus diagnostics on malformed
  input (panic-mode, token-level recovery to the next statement start).
  Recovered nodes are marked `invalid`. Deterministic: same input →
  same AST + diagnostics. This is the single most important requirement
  for the LSP, which parses live, mid-edit source.
- Diagnostic type carries `file:line:col` span, severity, message.
  Rendered text: `file:line:col: error: <message>` (GRAMMAR.md §11).

### 3.3 AST

- Typed, spans on every node, exactly one node type per grammar rule.
- Owned by `ulb-lang`; no internals leaked. Public types are documented
literal:  with rustdoc and doc-tests.

### 3.4 Evaluator

- Tree-walking interpreter over the AST. Resolves `convention`/`apply`,
  `fn` helpers, `env()`/`props()` builtins, version catalog references,
  `if`/`else`; produces a `ModuleModel`.
- Deterministic and side-effect-free except `env()`/`props()`.
- Errors are span-attached compile-time-style errors (unknown convention,
  unknown alias, type mismatch, missing env var) — never silent fallback.
- Role validation per GRAMMAR.md §10 (which statements are legal in which
  file role).

### 3.5 ModuleModel (evaluation output)

Per-module resolved configuration, ready for the task engine:

```
ModuleModel
├── module: ModuleName            (from settings.ulb module list)
├── plugins: [PluginRef]          (from libs.ulb plugins {} + build.ulb plugin "")
├── android: AndroidConfig        (Appendix A of GRAMMAR.md)
├── buildTypes: {name → BuildTypeConfig}
├── flavors: {name → FlavorConfig}, dimensions: [String]
├── signing: SigningConfig        (resolved from env()/props())
├── deps:
│   ├── module: {scope → [Dep]}           api/implementation/...
│   └── sourceSets: {name → {scope → [Dep]}}   commonMain.deps, androidMain.deps...
├── tasks: {name → TaskDef}       (dependsOn, run actions)
├── conventionsApplied: [String]
└── source: SourceSpan             (where this model came from)
```

The `ModuleModel` is the contract between the evaluator and every
downstream consumer (task engine, variant resolver, classpath builder).

---

## 4. Task graph & execution engine

Tasks are first-class graph nodes: `clean`, `test`, `lint`, `assemble`,
`bundle` (AAB), plus user-defined `task "name"` from `build.ulb`.

### 4.1 Model

- **Task** = name, module, inputs (file set + content-hash fingerprints),
  outputs (file set), dependencies (edges), run actions (closed action
  set: `copy`, allowlisted `exec`).
- The **task graph** is the transitive closure of `dependsOn` edges across
  all modules. Edges are directed, acyclic (cycle = resolution error with
  the cycle path in the message).

### 4.2 Execution model (decided: topo + parallel + fingerprinting)

1. **Topological order** over the full task DAG.
2. **Parallelism:** independent branches (no dependency relationship) run
   concurrently. Concurrency is bounded by a worker pool sized to
   `nproc`. Scheduling is deterministic: the DAG is partitioned into
   waves; within a wave, task order is stable (declaration order) so
   builds are reproducible.
3. **UP-TO-DATE:** a task is skipped iff every input fingerprint matches
   the recorded fingerprints of the last successful run (see §10) and all
   of its dependencies ran UP-TO-DATE. Otherwise it runs and records new
   fingerprints.
4. **Failure semantics:** a failing task marks the build failed; dependents
   are not started; already-scheduled independent tasks continue to
   completion (their outputs remain valid). No partial-success reporting.

### 4.3 Task run actions

The `run {}` body is a closed set of built-in actions (`copy`, allowlisted
`exec`) — no arbitrary code. `copy(from=..., to=...)` and
`exec(command=..., args=[...])` execute as part of the task, inside a
sandboxed working directory per module.

---

## 5. Build variants

### 5.1 Variant matrix

A variant = one build type × one flavor per dimension. With
`dimension "tier"` and flavors `free`/`paid`, and build types
`debug`/`release`, the matrix is `freeDebug`, `freeRelease`, `paidDebug`,
`paidRelease`. A flavor block may declare which dimension it belongs to
(GRAMMAR.md Appendix A); a variant is valid iff it contains exactly one
flavor per declared dimension.

Variant names follow `{flavor}{BuildType}` (e.g. `paidRelease`); the
"default" variant (no flavors) is just `debug`/`release`.

### 5.2 Per-variant source sets

Source sets layer over `src/main/kotlin`:

```
src/main/kotlin
src/{variant}/kotlin            (e.g. src/paidRelease/kotlin)
src/{flavor}/kotlin             (e.g. src/paid/kotlin)
src/{buildType}/kotlin          (e.g. src/release/kotlin)
```

Precedence (lowest → highest): `main` < `flavor` < `buildType` <
`variant`. The same layering applies to `res`, `AndroidManifest.xml` (see
§8), and `assets`.

### 5.3 Down-module variant propagation

When an app module is built for `paidRelease`, its library dependencies
must build for `release` (not `debug`). Rules:

- Propagate the **build type** down module edges (libraries have no
  flavors of their own by default).
- A library may itself declare flavors; then the app's flavor selection
  is matched by **flavor name** across dimensions, and build type by
  build type. Unmatched combinations are a resolution error naming both
  variants.
- The propagation is computed as part of variant resolution *before*
  any task is scheduled: each (module × variant) pair is a first-class
  node in the build plan.

---

## 6. Multi-module dependency graph

### 6.1 Graph model

`settings.ulb` declares the module list; each `build.ulb`'s
`deps {}` block declares project-module dependencies (syntax for module
references in `deps {}` is deferred to the module-graph phase — the
`ModuleModel.deps` shape already carries them).

### 6.2 `api` vs `implementation` classpath rules

- **`implementation`** dependencies of a module appear on that module's
  compile classpath and runtime classpath, but **not** on the compile
  classpath of its consumers.
- **`api`** dependencies appear on the module's compile/runtime classpath
  *and* are transitively visible on consumers' compile classpath.
- The compile classpath of a module = module deps (`api`+`implementation`)
  + transitively all `api` edges of those deps. The runtime classpath =
  transitive closure of `api`+`implementation` (with conflict resolution,
  see §10.3).

### 6.3 Parallel independent branches

Module subtrees with no dependency relationship are scheduled in parallel
by the task engine (§4.2). The module graph is computed as a DAG
(cycle → error with path), and the task DAG is derived from it.

---

## 7. KMP source-set hierarchy

Source sets form a tree, mirroring Kotlin's default hierarchy:

```
commonMain
├── androidMain
├── iosMain
└── desktopMain
```

- The default hierarchy matches Kotlin's published defaults; an explicit
  hierarchy can be declared in `build.ulb` (syntax deferred to the phase
  that implements KMP support — the `ModuleModel.sourceSets` field is
  already shaped for it).
- `deps {}` can be scoped per source set: `commonMain.deps { ... }`,
  `androidMain.deps { ... }` (GRAMMAR.md §6.4). Resolution rules follow
  the hierarchy: a dep declared on `commonMain` is visible to all
  platform source sets; a platform-scoped dep is visible only there.
- Compilation: each leaf source set compiles against its ancestors'
  outputs + deps; platform code sees common + platform deps, never a
  sibling platform's.

---

## 8. Manifest merging

For Android application/library modules, real manifest merging across
dependencies and variants, using AGP's precedence rules for the common
cases:

1. The **application** module's manifest wins over library manifests.
2. **Variant** manifest overlays **main** (`src/{variant}/AndroidManifest.xml`
   over `src/main/AndroidManifest.xml`).
3. Library manifests merge attributes/elements into the app manifest;
   conflicting explicit values are an error (no silent override) unless
   declared with a `tools:replace`-equivalent construct.
4. `minSdk`/`targetSdk`/`package` mismatches between modules are
   validation errors.

Manifest merging is a later phase; the pipeline reserves the stage (see
§12).

---

## 9. KSP / annotation processing

Compose and Room depend on KSP, so the compile pipeline needs a KSP step:

- `ksp(...)` dependencies (GRAMMAR.md Appendix B) form an
  annotation-processor classpath.
- Pipeline per module+variant: **generate** (run KSP) → **compile**
  (kotlinc) → **package**. KSP outputs are inputs to compilation.
- KSP runs *before* compilation of the same module and *after* its
  project dependencies have been compiled (their generated sources are
  available).
- Invokes the KSP compiler plugin as an external tool (this is the one
  place the pipeline shells out to the Kotlin ecosystem); the DSL itself
  never exposes arbitrary execution.

---

## 10. Dependency resolution, repositories & cache

### 10.1 Repositories

- Built-in defaults: **Google Maven** then **Maven Central**.
- `settings.ulb` `repositories { maven "url" }` entries are **additive**,
  tried in declared order *before* falling back to defaults (spec §12).

### 10.2 Cache layout

- Default artifact cache: `~/.cache/uliab/modules`, structured for
  content-addressed lookups (group/artifact/version + resolved files).
  Fully independent of Gradle's layout.
- **`lspCompat true`** (settings.ulb, opt-in): resolved artifacts are
  additionally **hardlinked** into
  `~/.gradle/caches/modules-2/files-2.1/...` so LSPs that expect Gradle's
  layout (e.g. kmp-lsp) can find them — no storage duplication, Gradle's
  layout never becomes the source of truth. (Hardlink = one inode, two
  directory entries; falls back to symlink/copy where filesystem forbids
  hardlinks, logged as a warning.)

### 10.3 Conflict resolution

Same group:artifact with different versions → highest version wins
(declared constraint order on tiebreak); conflicts are recorded as info
diagnostics. Exact conflicts between declared versions are surfaced, not
silently downgraded.

---

## 11. Editor tooling: tree-sitter vs LSP

The split is deliberate:

| Concern | Owner |
|---|---|
| syntax highlighting, folding, indentation, structural editing | `tree-sitter-ulb` |
| parse + semantic diagnostics, hover, goto-definition, completion | `ulb-lsp` |

The LSP does **not** use tree-sitter for analysis. It walks the same typed
AST + spans from `ulb-lang` that the evaluator uses, so semantic
diagnostics (unknown convention, unknown alias, unknown variant) are
guaranteed to match what evaluation would do — the LSP is a live preview
of the evaluator, not a parallel implementation.

### 11.1 `ulb-lsp` (tower-lsp, tokio)

Day-one capabilities (skeleton in Phase 5, full set per spec §5):

- **Diagnostics:** parse errors (as-you-type via
  `textDocument/didChange`, re-parse with the error-recovering parser);
  unknown convention / unknown alias / unknown identifier; unknown variant
  (flavor×buildType combination that does not exist); deprecation warnings
  (yellow squiggle pointing at the newer version when a referenced
  convention/plugin is deprecated).
- **Hover:** resolved value of a `libs.ulb` alias (actual
  group:artifact:version); resolved convention contents on `apply "name"`.
- **Go-to-definition:** `apply "android-app"` → the `convention
  android-app { }` block in `conventions.ulb`; alias references →
  `libs.ulb`.
- **Completion:** convention names, library aliases, DSL keywords, and
  block-scoped valid keys (inside `android {}` only android keys).

Deprioritized (future, not built this pass): rename-symbol, find-all-
references across large workspaces, semantic tokens beyond basic
tree-sitter highlighting.

### 11.2 Grammar sync-by-hand risk (known tradeoff)

`tree-sitter-ulb/grammar.js` is maintained by hand against
`Uliab/GRAMMAR.md` — there is no generated shared source of truth. This is
accepted for this pass (spec §4). Mitigations:

- GRAMMAR.md is written specifically to be mechanically portable (§8 of
  GRAMMAR.md), so drift is cheap to detect.
- Any grammar change in `Uliab` produces a `PROGRESS.md` entry in
literal:  `tree-sitter-ulb` (and vice versa).
- The `tree-sitter-ulb` test suite parses the same example files as
  `ulb-lang`'s snapshot tests, so a mismatch fails loudly.

---

## 12. End-to-end build pipeline

```
1. Parse+eval settings.ulb         → project name, modules, repos, lspCompat
2. Parse+eval libs.ulb             → version catalog (aliases, bundles, plugins)
3. Parse+eval conventions.ulb      → convention + fn tables
4. For each module: parse+eval build.ulb → ModuleModel (apply, if/env, props)
5. Validate role rules (GRAMMAR.md §10) + cross-module references
6. Compute variant matrix (build types × flavors), validate
7. Propagate variants down module edges (§5.3)
8. Build module graph, apply api/implementation classpath rules (§6)
9. Resolve external deps against repos → cache (+ optional lspCompat links)
10. Derive task DAG; fingerprint inputs; schedule waves (topo, parallel, UP-TO-DATE)
11. Per module+variant: KSP generate → compile → package; manifest merge (§8)
12. assemble/bundle outputs; record fingerprints for next build
```

Steps 1–9 are the "configuration phase" (deterministic, side-effect-free
except cache reads/writes). Steps 10–12 are the "execution phase".

---

## 13. Fingerprinting & UP-TO-DATE caching

- Every `.ulb` file is content-hashed (SHA-256) at configuration time.
- A task's **input fingerprint** = hash of (its module's resolved
  `ModuleModel` + every `.ulb` file it depends on through conventions/
  catalog + the resolved dependency graph + its file inputs + the
  compiler/plugin versions). Content-addressed, not timestamp-based.
- Outputs are recorded with the fingerprint that produced them; a task is
  UP-TO-DATE when its current input fingerprint equals the recorded one
  and all dependencies are UP-TO-DATE (§4.2). This is designed in from
  the start (spec §14), not bolted on.
- No remote cache this pass; the fingerprint format should not preclude
  one later.

---

## 14. Crate/phase roadmap

| Phase | Scope | Deliverable |
|---|---|---|
| 1 (done) | GRAMMAR.md + ARCHITECTURE.md | design lock, review |
| 2 | `ulb-lang` workspace + lexer + AST | tokens, spans, one test per construct |
| 3 | `ulb-lang` parser + error recovery | partial AST + diagnostics, snapshot + malformed tests |
| 4 | `ulb-lang` evaluator + worked example | ModuleModel, conventions/fn/env, end-to-end example |
| 5 | `tree-sitter-ulb` | grammar.js + highlights/folds/indents, proven on same fixtures |
| 6 | `ulb-lsp` skeleton | tower-lsp: didChange parse diagnostics + unknown-convention |
| 7+ | CLI + task engine, variants, module graph, KMP, manifest merge, KSP, resolver/cache, fingerprinting | production pipeline (designed above) |

Phases 2–6 are sequential because each depends on the previous; 7+ can be
partly parallelized once `ulb-lang` is stable.

---

## 15. Out of scope & deferred (explicit)

From spec §"Explicitly OUT of scope": Gradle/.kts compat, Kotlin-scripting
bridge, kotlinc in the tool, third-party plugin ecosystem, general Android
compat, remote cache, publishing.

From spec §5: LSP rename, find-all-references, advanced semantic tokens.

Deferred to later phases (tracked in PROGRESS.md, designed above):
explicit KMP hierarchy declaration syntax (§7), module-project dep
declaration syntax in `deps {}` (§6), manifest merge, KSP step, resolver
+ cache + lspCompat links, task engine, CLI.
