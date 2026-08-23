# Maven dependency resolution (`crates/uliab/src/maven.rs`)

The core owns dependency resolution so every jvm-family plugin shares one
classpath model. Plugins never resolve Maven coordinates themselves; they
receive the resolved buckets in their `configure` JSON.

## Repositories

```
enum MavenRepo { Google, Central, Custom(String) }
```

- `Google` — `https://dl.google.com/dl/android/maven2`
- `Central` — `https://repo1.maven.org/maven2`
- `Custom(String)` — any URL or local path

`url_for` builds the standard `group/artifact/version/…` layout. Custom
repositories are prepended in declared order ahead of Google and Central;
the `--repo` flag on `uliab build` / `uliab deps resolve` feeds
`repos_for`, which both commands share. `file://` and plain-path
repositories let resolution run offline and be tested without a network.

## POM parsing

POMs are parsed with `quick-xml`. What is captured:

- `packaging`,
- groupId / artifactId / version,
- scope, `optional`,
- each child `dependency`,
- `dependencyManagement` entries (version constraints from BOMs).

Scope mapping:

| POM scope | Effect |
|---|---|
| `runtime` | `PomScope::Runtime` |
| `test`, `provided`, `system` | dependency is dropped (`Skip`) |
| anything else (default `compile`) | `Compile` |

## BOM / `dependencyManagement` support

POMs with `packaging = "pom"` are treated as BOMs (Bills of Materials).
Their `dependencyManagement` section is parsed and recorded as version
constraints. When a child dependency has no version (declared as
`"group:artifact"` in `deps {}`), or its version contains a `${property}`
that cannot be resolved from the POM alone, the resolver looks up the
managed version from active BOMs.

Resolution order:

1. Declared deps with explicit versions are expanded first.
2. BOMs encountered during expansion populate the managed-version map.
3. Version-less declared deps are expanded in a second pass, using the
   now-populated managed versions.
4. The first BOM to declare a constraint for a given `group:artifact`
   wins (nearest definition wins, matching Maven semantics).

Parent POM inheritance is **not** consulted yet.

## Transitive expansion and conflict resolution

Expansion is transitive from the module's declared deps. For each
`group:artifact`, exactly one version survives:

- the **highest** version wins;
- ties (equal versions) break in declared-constraint order;
- conflicts are recorded as info, never silently downgraded.

Version ordering is alphanumeric with the release-queue ordering
`alpha < beta < milestone < rc < snapshot < release < sp`, which keeps
`1.0.0-rc1 < 1.0.0 < 1.0.1-sp1` ordering sane.

## Scope buckets

The winning nodes are partitioned into buckets mirroring GRAMMAR.md
Appendix B:

```
compile / runtime / processor
test_compile / test_runtime
android_test_compile / android_test_runtime
```

The classpath-visibility rules that matter to consumers:

- **compile** classpath = module deps + transitive `api` edges;
- **runtime** classpath = the transitive `api` + `implementation` closure;
- test buckets are separate, so `testImplementation` jars never leak onto
  a main compile classpath — the `jvm-scoped-classpath` CI job in
  `ulb-plugins` exists specifically to prove that.

## Caching

Artifacts are cached content-addressed under
`~/.cache/uliab/modules`, keyed by the artifact itself. A cached jar is
reused only when the recorded SHA-256 still matches the file on disk.

## Not yet implemented (tracked in architecture.md §12)

- parent POM inheritance,
- property-versioned children without a BOM (currently skipped with a
  note).
