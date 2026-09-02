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

## AAR materialization

Android libraries are published as AARs: a zip whose real compile payload
is an inner `classes.jar`. When a declared or transitive dependency's POM
has `packaging = "aar"`, the resolver downloads the AAR and extracts that
`classes.jar` into the artifact's cache directory, contributing the
extracted jar to the classpath rather than the archive itself. The
extracted jar is tied to the exact AAR bytes it was derived from, so a
refetched or re-verified AAR (for example a changed `-SNAPSHOT`)
invalidates a stale extraction on the next build.

## BOM / `dependencyManagement` support

POMs with `packaging = "pom"` are treated as BOMs (Bills of Materials).
Their `dependencyManagement` section is parsed and recorded as version
constraints. When a child dependency has no version (declared as
`"group:artifact"` in `deps {}`), or its version contains a `${property}`
that cannot be resolved from the POM alone, the resolver looks up the
managed version from active BOMs.

Every POM also resolves its own version-less dependencies from its own
`dependencyManagement`, regardless of packaging. This is what lets an AAR
(non-BOM) pin one of its dependencies in its own `dependencyManagement`
while declaring that dependency with the Gradle `unspecified` placeholder
(`<version>unspecified</version>`, meaning "version comes from
`dependencyManagement`"): the placeholder is treated as version-less and
resolved against the owning POM's management before any BOM constraint.

POM version normalization: a `<version>` of the Maven hard-pin form
(`<version>[1.2.3]</version>`) resolves to the pinned `1.2.3` — the
brackets are pin syntax, not part of the version. Inclusive range pins
(which carry a comma, e.g. `[1.0,2.0]`) are left as a range and fail to
resolve, since a range is not a single concrete version.

Resolution order:

1. Declared deps with explicit versions are expanded first.
2. BOMs encountered during expansion populate the managed-version map.
3. Version-less declared deps are expanded in a second pass, using the
   now-populated managed versions.
4. Within a POM, its own `dependencyManagement` constraints are consulted
   before BOM-managed versions (nearest definition wins).
5. The first BOM to declare a constraint for a given `group:artifact`
   wins (nearest definition wins, matching Maven semantics).

Parent POM inheritance is **not** consulted yet.

## KMP Android-variant substitution

AndroidX Compose and the rest of the Android KMP ecosystem publish each
library twice: a *metadata-stub* base artifact and a real `-android`
variant. The base `aar` is empty (it carries no `classes.jar`); Gradle
normally maps a dependency onto the `-android` variant by reading the
artifact's `.module` (Gradle module metadata), which a POM-only resolver
cannot do. When resolving for an **Android target** the resolver instead
substitutes each base coordinate with its `-android` sibling.

This is opt-in via `Resolver::with_android_variants(true)` — the driver
enables it for modules that declare an `android {}` block. Plain-JVM
(`jvm {}`) modules target desktop and must **not** substitute, since
`org.jetbrains.compose` artifacts follow a separate distribution.

Selection happens during graph expansion, per `group:artifact:version`:
when the coordinate is an `aar`, is not already named `-android`, and a
same-version `-android` sibling exists whose `aar` actually carries a
`classes.jar`, the sibling's POM is expanded (its transitive children join
the graph) and the base coordinate is resolved to the sibling's archive.

- The winning coordinate stays keyed by the **base** `group:artifact`, so
  `root_paths` callers that look up `runtime`/`ui`/`material3` are
  unaffected; only the archive fetch is redirected to the sibling.
- Materializing an empty stub under this mode contributes nothing and
  records an explanatory note rather than failing the whole resolution.
- With the flag **off** the behavior is unchanged and strict: an `aar`
  lacking `classes.jar` is a hard `ArchiveError`.

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
