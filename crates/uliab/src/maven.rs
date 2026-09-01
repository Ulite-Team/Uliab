//! The Maven dependency resolver: turns a project's `deps {}` block into a
//! concrete classpath (ARCHITECTURE.md §6, §7). Plain jars contribute their
//! own file; Android AARs contribute the `classes.jar` extracted from the
//! archive, so an `androidx.compose.*` dependency puts its `@Composable`
//! API classes on the compile classpath.
//!
//! A [`Resolver`] expands the declared dependencies transitively by
//! downloading POMs from its repository list, picks one version per
//! `group:artifact` by the highest-version rule (§7.3), and partitions the
//! surviving nodes into classpath buckets that mirror the Gradle scope
//! semantics of GRAMMAR.md Appendix B:
//!
//! - `compile` — declared `api`/`implementation`/`compileOnly` plus the
//!   compile-scope children of `api`/`implementation` deps, transitively;
//!   `compileOnly` deps contribute only their own jar (§6.2).
//! - `runtime` — the full transitive closure of `api`/`implementation`/
//!   `runtimeOnly` deps.
//! - `processor` — `ksp` deps and their closure.
//! - `test_compile`/`test_runtime` — `testImplementation` deps layered on
//!   top of the main `compile`/`runtime` classpaths.
//! - `android_test_compile`/`android_test_runtime` — the same for
//!   `androidTestImplementation`.
//!
//! The `api` vs `implementation` distinction only changes what consumers of
//! a module see, which is realized at the multi-module layer (§6.1); for a
//! single module both contribute the same jars to its own classpaths.
//!
//! Version comparison follows Maven's ordering: dot/hyphen-separated
//! numeric segments compare by value and the common qualifiers rank
//! `alpha < beta < milestone < rc < snapshot < release < sp`. A POM child
//! whose version is absent is resolved from `dependencyManagement` entries
//! provided by BOMs (POMs with `packaging = "pom"`) in the dependency
//! graph. A child whose version is a `${property}` is resolved from the
//! same managed versions when available; parent POM inheritance lies
//! outside the resolver's scope.
//!
//! Artifacts are cached content-addressed under the resolver's cache
//! directory (default `~/.cache/uliab/modules`): a jar/POM is only reused
//! from the cache when its recorded SHA-256 matches the file on disk, and a
//! mismatch triggers a refetch. A download streams through a temporary
//! `.part` file while its SHA-256 is hashed incrementally, so an artifact
//! is never buffered in memory and only an intact, digest-matched file
//! ever appears in the cache. Repositories are tried in order, so a local
//! filesystem repository (plain path or `file://`) in front of the default
//! repositories keeps resolution offline for tests.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64};

use sha2::{Digest, Sha256};

use ulb_lang::eval::Value;

/// The default Google Maven repository (ARCHITECTURE.md §7.1).
pub const GOOGLE_MAVEN: &str = "https://dl.google.com/dl/android/maven2";
/// The default Maven Central repository (ARCHITECTURE.md §7.1).
pub const MAVEN_CENTRAL: &str = "https://repo1.maven.org/maven2";

/// A repository a resolver can fetch POMs and jars from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MavenRepo {
    /// The Google Maven repository (`https://dl.google.com/dl/android/maven2`).
    Google,
    /// Maven Central (`https://repo1.maven.org/maven2`).
    Central,
    /// An explicit repository: an `https://`/`http://` URL, a `file://`
    /// URL, or a plain filesystem path. Repositories are tried in order,
    /// so a local repository placed first keeps resolution offline.
    Custom(String),
}

impl MavenRepo {
    fn base(&self) -> &str {
        match self {
            MavenRepo::Google => GOOGLE_MAVEN,
            MavenRepo::Central => MAVEN_CENTRAL,
            MavenRepo::Custom(url) => url,
        }
    }

    /// The repository-relative layout Maven uses: `group/artifact/version/…`.
    fn url_for(&self, rel_path: &str) -> String {
        let base = self.base();
        if base.ends_with('/') {
            format!("{base}{rel_path}")
        } else {
            format!("{base}/{rel_path}")
        }
    }
}

/// A dependency scope in `deps {}` (GRAMMAR.md Appendix B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavenScope {
    /// Visible to this module's compile classpath and to consumers.
    Api,
    /// Visible to this module's compile and runtime classpaths only.
    Implementation,
    /// Runtime classpath only.
    RuntimeOnly,
    /// Compile classpath only, no transitive children.
    CompileOnly,
    /// Annotation-processing (KSP) classpath.
    Ksp,
    /// Unit-test classpaths.
    TestImplementation,
    /// Instrumented-test classpaths.
    AndroidTestImplementation,
}

impl MavenScope {
    /// Parses a `deps {}` key into a scope.
    pub fn from_name(name: &str) -> Option<MavenScope> {
        match name {
            "api" => Some(MavenScope::Api),
            "implementation" => Some(MavenScope::Implementation),
            "runtimeOnly" => Some(MavenScope::RuntimeOnly),
            "compileOnly" => Some(MavenScope::CompileOnly),
            "ksp" => Some(MavenScope::Ksp),
            "testImplementation" => Some(MavenScope::TestImplementation),
            "androidTestImplementation" => Some(MavenScope::AndroidTestImplementation),
            _ => None,
        }
    }
}

/// A resolved Maven coordinate. The version may be empty when the
/// coordinate was declared without one (e.g. `"group:artifact"`) and is
/// expected to be filled in from a BOM's `dependencyManagement` during
/// resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The Maven group.
    pub group: String,
    /// The Maven artifact.
    pub artifact: String,
    /// The version. Empty when the coordinate was declared without one
    /// and should be resolved from a BOM during dependency resolution.
    pub version: String,
}

impl Dependency {
    /// Parses a `"group:artifact:version"` or `"group:artifact"` coordinate
    /// string.
    ///
    /// Two-part coordinates (`"group:artifact"`) produce a [`Dependency`]
    /// with an empty version, intended to be resolved from a BOM's
    /// `dependencyManagement` during expansion.
    ///
    /// # Errors
    ///
    /// Returns a description when the string is not two or three
    /// `:`-separated non-empty parts, or when it has more than three parts.
    pub fn parse(coordinate: &str) -> Result<Dependency, String> {
        let mut parts = coordinate.split(':');
        let group = parts.next().unwrap_or_default();
        let artifact = parts.next().unwrap_or_default();
        let version_part = parts.next();
        let version = version_part.unwrap_or_default();
        if parts.next().is_some()
            || group.is_empty()
            || artifact.is_empty()
            || version_part.is_some_and(str::is_empty)
        {
            return Err(format!(
                "invalid coordinate '{coordinate}': expected 'group:artifact' or \
                 'group:artifact:version'"
            ));
        }
        Ok(Dependency {
            group: group.to_owned(),
            artifact: artifact.to_owned(),
            version: version.to_owned(),
        })
    }

    /// Returns `true` when the coordinate was declared without a version
    /// and needs resolution from a BOM during dependency expansion.
    #[must_use]
    pub fn is_version_managed(&self) -> bool {
        self.version.is_empty()
    }
}

/// A declared dependency and the scope it was declared under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredDep {
    /// The scope this dependency belongs to.
    pub scope: MavenScope,
    /// The resolved coordinate.
    pub dependency: Dependency,
}

/// Parses the evaluated `deps {}` block of a module model.
///
/// The block is keyed by scope (Appendix B); each value is a coordinate
/// string (`"group:artifact:version"` or `"group:artifact"` for
/// BOM-managed deps), a resolved [`Value::Coordinate`], or a list of them
/// (repeated keys accumulate into lists upstream).
///
/// # Errors
///
/// Returns a description when a scope is unknown, a value is not a
/// coordinate, or a coordinate is malformed.
pub fn parse_deps_block(block: &Value) -> Result<Vec<DeclaredDep>, String> {
    let entries = match block {
        Value::Block(entries) => entries,
        _ => return Err("deps must be a block".to_owned()),
    };
    let mut deps = Vec::new();
    for (scope_name, value) in entries {
        let scope = MavenScope::from_name(scope_name).ok_or_else(|| {
            format!(
                "unknown dependency scope '{scope_name}' (expected api, implementation, \
                 runtimeOnly, compileOnly, ksp, testImplementation, or androidTestImplementation)"
            )
        })?;
        match value {
            Value::List(items) => {
                for item in items {
                    if matches!(item, Value::ProjectRef(_)) {
                        continue;
                    }
                    deps.push(DeclaredDep {
                        scope,
                        dependency: parse_coordinate_value(item, scope_name)?,
                    });
                }
            }
            Value::ProjectRef(_) => continue,
            other => deps.push(DeclaredDep {
                scope,
                dependency: parse_coordinate_value(other, scope_name)?,
            }),
        }
    }
    Ok(deps)
}

/// Extracts project-module dependencies from a `deps {}` block.
///
/// Walks every scope entry and collects `(scope, module_path)` pairs for
/// each [`Value::ProjectRef`] found — as single values or inside a
/// [`Value::List`]. Non-project-ref entries are ignored; the caller
/// resolves those via [`parse_deps_block`] and the Maven resolver.
///
/// The module path retains its leading `:` (e.g. `":shared"`).
///
/// # Examples
///
/// ```
/// use ulb_lang::eval::Value;
/// use uliab::maven::{extract_project_deps, MavenScope};
///
/// let deps = Value::Block(
///     [(
///         "implementation".to_owned(),
///         Value::ProjectRef(":shared".to_owned()),
///     )]
///     .into_iter()
///     .collect(),
/// );
/// let refs = extract_project_deps(&deps);
/// assert_eq!(refs, vec![(MavenScope::Implementation, ":shared".to_owned())]);
/// ```
pub fn extract_project_deps(block: &Value) -> Vec<(MavenScope, String)> {
    let entries = match block {
        Value::Block(entries) => entries,
        _ => return Vec::new(),
    };
    let mut refs = Vec::new();
    for (scope_name, value) in entries {
        let Some(scope) = MavenScope::from_name(scope_name) else {
            continue;
        };
        match value {
            Value::ProjectRef(path) => {
                refs.push((scope, path.clone()));
            }
            Value::List(items) => {
                for item in items {
                    if let Value::ProjectRef(path) = item {
                        refs.push((scope, path.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    refs
}

fn parse_coordinate_value(value: &Value, scope: &str) -> Result<Dependency, String> {
    let coordinate = match value {
        Value::Str(text) => text,
        Value::Coordinate(text) => text,
        other => {
            return Err(format!(
                "dependency in '{scope}' must be a coordinate string, found {}",
                kind(other)
            ));
        }
    };
    Dependency::parse(coordinate)
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Str(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Version(_) => "version",
        Value::Properties(_) => "properties",
        Value::Coordinate(_) => "coordinate",
        Value::Block(_) => "block",
        Value::Invalid(_) => "an unresolved value",
        Value::ProjectRef(_) => "project reference",
    }
}

/// The classpath a project's dependencies resolve to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Classpath {
    /// Jars needed to compile the module.
    pub compile: Vec<PathBuf>,
    /// Jars needed to run the module.
    pub runtime: Vec<PathBuf>,
    /// Jars needed to run annotation processing.
    pub processor: Vec<PathBuf>,
    /// Direct `api`-scoped jars: needed for compilation and visible to
    /// consumers of this module.  Unlike [`compile`](Self::compile), this
    /// contains **only** the direct `api` roots (not their transitive
    /// dependencies), so consumers can build a compile classpath without
    /// pulling in `implementation`-only deps.
    pub api: Vec<PathBuf>,
    /// The Android Compose runtime artifacts the driver bundles with a
    /// compiled Android module that uses `@Composable`: the `classes.jar`
    /// of the `androidx.compose.runtime`, `androidx.compose.ui`, and
    /// `androidx.compose.material3` artifacts, exactly as resolved from
    /// the BOM.  These are a subset of [`compile`](Self::compile) — they
    /// appear there too — but kept separate so an Android linker (d8) can
    /// add exactly the runtime types the `@Composable` lowering emits
    /// without scanning a flat classpath by filename.  This bucket is
    /// produced by the host's `driver::compose_runtime_paths` — the
    /// resolver itself leaves it empty, since the resolver cannot know
    /// which resolved coordinates a caller treats as "Compose".  It stays
    /// empty when the module does not use Compose or none of the runtime
    /// artifacts resolved.
    pub compose_runtimes: Vec<PathBuf>,
    /// Jars needed to compile unit tests.
    pub test_compile: Vec<PathBuf>,
    /// Jars needed to run unit tests.
    pub test_runtime: Vec<PathBuf>,
    /// Jars needed to compile instrumented tests.
    pub android_test_compile: Vec<PathBuf>,
    /// Jars needed to run instrumented tests.
    pub android_test_runtime: Vec<PathBuf>,
}

impl Classpath {
    /// Serializes the classpath to the JSON object handed to plugins as the
    /// `classpath` key of their configuration: one array of jar paths per
    /// bucket, named as they appear here (`compile`, `runtime`,
    /// `processor`, `api`, `composeRuntimes`, `testCompile`,
    /// `testRuntime`, `androidTestCompile`, `androidTestRuntime`). The
    /// output is deterministic: each bucket is already sorted by
    /// (`group`, `artifact`) when built by [`Resolver`].
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let path_list = |paths: &[PathBuf]| -> Vec<serde_json::Value> {
            paths
                .iter()
                .map(|path| serde_json::Value::String(path.display().to_string()))
                .collect()
        };
        serde_json::json!({
            "compile": path_list(&self.compile),
            "runtime": path_list(&self.runtime),
            "processor": path_list(&self.processor),
            "api": path_list(&self.api),
            "composeRuntimes": path_list(&self.compose_runtimes),
            "testCompile": path_list(&self.test_compile),
            "testRuntime": path_list(&self.test_runtime),
            "androidTestCompile": path_list(&self.android_test_compile),
            "androidTestRuntime": path_list(&self.android_test_runtime),
        })
    }
}

/// The outcome of a [`Resolver::resolve`] run: the classpath plus
/// informational notes (conflicts resolved, children skipped, unsupported
/// packaging) that a caller may surface to the user.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The resolved classpath.
    pub classpath: Classpath,
    /// The resolved path of each direct declared `deps {}` root coordinate
    /// (keyed by `(group, artifact)`) that materialized to a jar or an AAR
    /// `classes.jar`.  Coordinates that carry no artifact — a BOM, say, or
    /// a dep that did not resolve — are absent.  This lets a caller that
    /// injected extra declared deps (the host, for the Compose runtime
    /// artifacts) resolve exactly which jar a known coordinate produced,
    /// instead of scanning a flattened classpath bucket by filename.
    pub root_paths: RootPaths,
    /// Human-readable notes about choices made during resolution.
    pub notes: Vec<String>,
}

/// The resolved file a direct declared root coordinate (`(group, artifact)`)
/// materialized to, as returned in [`Resolution::root_paths`].
pub type RootPaths = BTreeMap<(String, String), PathBuf>;

/// Errors produced while resolving dependencies.
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// An artifact could not be found in any configured repository.
    NotFound {
        /// The `group:artifact:version` coordinate that was not found.
        artifact: String,
    },
    /// Fetching or reading an artifact failed for a non-404 reason.
    Fetch {
        /// The coordinate being fetched when the failure occurred.
        artifact: String,
        /// What went wrong.
        message: String,
    },
    /// A POM could not be parsed.
    Parser {
        /// The coordinate whose POM was malformed.
        artifact: String,
        /// What about the POM could not be parsed.
        message: String,
    },
    /// An archive (e.g. an Android AAR) could not be read or unpacked.
    Archive {
        /// The coordinate whose archive is malformed.
        artifact: String,
        /// What about the archive could not be read.
        message: String,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound { artifact } => {
                write!(formatter, "could not find {artifact} in any repository")
            }
            ResolveError::Fetch { artifact, message } => {
                write!(formatter, "fetching {artifact}: {message}")
            }
            ResolveError::Parser { artifact, message } => {
                write!(formatter, "parsing POM for {artifact}: {message}")
            }
            ResolveError::Archive { artifact, message } => {
                write!(formatter, "unpacking archive for {artifact}: {message}")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolves `deps {}` declarations into a classpath (ARCHITECTURE.md §6,
/// §7). Repositories are consulted in order; artifacts are cached under
/// `cache_dir` (default `~/.cache/uliab/modules`) and reused only when
/// their recorded SHA-256 still matches.
#[derive(Debug, Clone)]
pub struct Resolver {
    repos: Vec<MavenRepo>,
    cache_dir: PathBuf,
}

impl Resolver {
    /// Creates a resolver that consults `repos` in order and caches under
    /// `cache_dir` (or the default cache location when `None`).
    pub fn new(repos: Vec<MavenRepo>, cache_dir: Option<PathBuf>) -> Resolver {
        Resolver {
            repos,
            cache_dir: cache_dir.unwrap_or_else(default_cache_dir),
        }
    }

    /// Resolves `declared` into a classpath, downloading POMs and jars
    /// into the cache on a miss.
    ///
    /// The graph of all reachable versions is expanded first and then
    /// collapsed by the highest-version rule (§7.3), so a transitive
    /// dependency of a dependency participates in conflict resolution like
    /// any other occurrence of its `group:artifact`.
    ///
    /// `resolve` is a pure function of its arguments and the resolver's
    /// repositories and cache directory: each call walks a fresh graph and
    /// retains no mutable state between calls, so a single resolver may be
    /// reused across independent declarations.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::NotFound`] when an artifact is absent from
    /// every repository, [`ResolveError::Fetch`] when a download or cache
    /// read fails, and [`ResolveError::Parser`] when a POM is malformed.
    ///
    /// # Examples
    ///
    /// A local repository carries `example:one:1.0`, whose POM depends on
    /// `example:two:1.0`. Declaring `one` as an `implementation` dependency
    /// puts both jars on the compile classpath (the compile-scope child of
    /// an `implementation` dep is visible to this module, §6.2) and on
    /// runtime:
    ///
    /// ```rust
    /// use std::fs;
    /// use uliab::maven::{DeclaredDep, Dependency, MavenRepo, MavenScope, Resolver};
    ///
    /// let dir = std::env::temp_dir().join(format!(
    ///     "uliab-maven-doc-{}", std::process::id()
    /// ));
    /// let _ = fs::remove_dir_all(&dir);
    /// let repo = dir.join("repo");
    /// let one_pom = "com/example/one/1.0/one-1.0.pom";
    /// let two_pom = "com/example/two/1.0/two-1.0.pom";
    /// fs::create_dir_all(repo.join("com/example/one/1.0")).unwrap();
    /// fs::create_dir_all(repo.join("com/example/two/1.0")).unwrap();
    /// fs::write(repo.join(one_pom), r#"<?xml version="1.0"?>
    /// <project><modelVersion>4.0.0</modelVersion>
    /// <groupId>com.example</groupId><artifactId>one</artifactId><version>1.0</version>
    /// <dependencies><dependency>
    ///   <groupId>com.example</groupId><artifactId>two</artifactId><version>1.0</version>
    /// </dependency></dependencies></project>"#).unwrap();
    /// fs::write(repo.join(two_pom), r#"<?xml version="1.0"?>
    /// <project><modelVersion>4.0.0</modelVersion>
    /// <groupId>com.example</groupId><artifactId>two</artifactId><version>1.0</version>
    /// </project>"#).unwrap();
    /// fs::write(repo.join("com/example/one/1.0/one-1.0.jar"), b"one").unwrap();
    /// fs::write(repo.join("com/example/two/1.0/two-1.0.jar"), b"two").unwrap();
    ///
    /// let resolver = Resolver::new(
    ///     vec![MavenRepo::Custom(repo.display().to_string())],
    ///     Some(dir.join("cache")),
    /// );
    /// let declared = vec![DeclaredDep {
    ///     scope: MavenScope::Implementation,
    ///     dependency: Dependency::parse("com.example:one:1.0").unwrap(),
    /// }];
    /// let resolution = resolver.resolve(&declared).expect("resolves");
    /// let jars: Vec<_> = resolution.classpath.compile.iter()
    ///     .filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
    ///     .collect();
    /// assert_eq!(jars, vec!["one-1.0.jar", "two-1.0.jar"]);
    /// ```
    pub fn resolve(&self, declared: &[DeclaredDep]) -> Result<Resolution, ResolveError> {
        let mut session = Session::new(self);

        // Pass 1: expand every declared dep that already carries a version.
        // BOMs (packaging = "pom") discovered here populate
        // `managed_versions` so pass 2 can fill in version-less deps.
        for dep in declared {
            if !dep.dependency.is_version_managed() {
                session.expand(
                    &dep.dependency.group,
                    &dep.dependency.artifact,
                    &dep.dependency.version,
                )?;
            }
        }

        // Pass 2: expand version-managed deps using BOM constraints.
        for dep in declared {
            if dep.dependency.is_version_managed() {
                if let Some(version) = session.managed_versions.get(&(
                    dep.dependency.group.clone(),
                    dep.dependency.artifact.clone(),
                )) {
                    let version = version.clone();
                    session.expand(&dep.dependency.group, &dep.dependency.artifact, &version)?;
                } else {
                    session.notes.push(format!(
                        "{}:{} has no version and no BOM provides one; it is skipped",
                        dep.dependency.group, dep.dependency.artifact
                    ));
                }
            }
        }

        let winners = session.winners();
        let (classpath, root_paths) = session.classpath(declared, &winners)?;
        Ok(Resolution {
            classpath,
            root_paths,
            notes: session.notes,
        })
    }

    fn fetch_cached(
        &self,
        group: &str,
        artifact: &str,
        version: &str,
        extension: &str,
    ) -> Result<PathBuf, ResolveError> {
        let rel = format!(
            "{}/{}/{}/{}-{}.{}",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version,
            extension
        );
        let cache_file = self.cache_dir.join(&rel);
        if verified_cached(&cache_file) {
            return Ok(cache_file);
        }
        self.fetch_from_repos(&rel, &cache_file)?;
        Ok(cache_file)
    }

    /// Downloads an Android AAR and returns the path to the `classes.jar`
    /// extracted from it, which is what an AAR contributes to a compile
    /// classpath. The AAR (a zip) is fetched and verified through
    /// `fetch_cached`; the extracted `classes.jar` is written beside it in
    /// the artifact's cache directory.
    ///
    /// The extracted jar is tied to the AAR it came from: each extraction
    /// records the AAR's digest beside it, and a later run re-extracts
    /// whenever that digest no longer matches the currently-cached AAR.
    /// This keeps the jar correct when a mutable coordinate (a `-SNAPSHOT`,
    /// or a custom repo serving changing bytes) refetches different AAR
    /// bytes, and lets a corrupted jar self-heal on the next build.
    ///
    /// Extraction writes through a process-unique `<name>.part-<pid>` file
    /// and renames it into place atomically, so concurrent processes
    /// extracting the same coordinate never interleave writes, and the
    /// copy is size-capped like the download path so a pathological
    /// `classes.jar` cannot grow the cache beyond the artifact byte limit.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::Fetch`] when the AAR cannot be downloaded
    /// and [`ResolveError::Archive`] when the AAR cannot be read, is not a
    /// zip, lacks a `classes.jar` entry, or its `classes.jar` exceeds the
    /// size limit.
    fn materialize_aar(
        &self,
        group: &str,
        artifact: &str,
        version: &str,
    ) -> Result<PathBuf, ResolveError> {
        let aar_path = self.fetch_cached(group, artifact, version, "aar")?;
        let classes_path = self.cache_dir.join(format!(
            "{}/{}/{}/{}-{}-classes.jar",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version
        ));
        let aar_digest = read_recorded_digest(&aar_path);
        if classes_path.is_file() && read_recorded_digest(&classes_path) == aar_digest {
            return Ok(classes_path);
        }
        let coordinate = format!("{group}:{artifact}:{version}");
        let file = std::fs::File::open(&aar_path).map_err(|error| ResolveError::Archive {
            artifact: coordinate.clone(),
            message: format!("opening {}: {error}", aar_path.display()),
        })?;
        let mut archive = zip::ZipArchive::new(file).map_err(|error| ResolveError::Archive {
            artifact: coordinate.clone(),
            message: format!("reading {}: {error}", aar_path.display()),
        })?;
        let mut entry = archive
            .by_name("classes.jar")
            .map_err(|error| ResolveError::Archive {
                artifact: coordinate.clone(),
                message: format!("no classes.jar in {}: {error}", aar_path.display()),
            })?;
        if let Some(parent) = classes_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| ResolveError::Archive {
                artifact: coordinate.clone(),
                message: format!("creating {}: {error}", parent.display()),
            })?;
        }
        let part_path = PathBuf::from(format!(
            "{}.part-{}-{}",
            classes_path.display(),
            std::process::id(),
            PART_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let outcome = (|| -> Result<(), ResolveError> {
            let mut out =
                std::fs::File::create(&part_path).map_err(|error| ResolveError::Archive {
                    artifact: coordinate.clone(),
                    message: format!("creating {}: {error}", part_path.display()),
                })?;
            let mut copied = 0u64;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let read = entry
                    .read(&mut buffer)
                    .map_err(|error| ResolveError::Archive {
                        artifact: coordinate.clone(),
                        message: format!("reading {}: {error}", part_path.display()),
                    })?;
                if read == 0 {
                    break;
                }
                copied += read as u64;
                if copied > MAX_ARTIFACT_BYTES {
                    return Err(ResolveError::Archive {
                        artifact: coordinate.clone(),
                        message: format!(
                            "classes.jar in {} exceeds the {MAX_ARTIFACT_BYTES} byte limit",
                            aar_path.display()
                        ),
                    });
                }
                out.write_all(&buffer[..read])
                    .map_err(|error| ResolveError::Archive {
                        artifact: coordinate.clone(),
                        message: format!("writing {}: {error}", part_path.display()),
                    })?;
            }
            out.sync_all().map_err(|error| ResolveError::Archive {
                artifact: coordinate.clone(),
                message: format!("writing {}: {error}", part_path.display()),
            })?;
            drop(out);
            if let Some(digest) = &aar_digest {
                std::fs::write(sha_path(&classes_path), digest).map_err(|error| {
                    ResolveError::Archive {
                        artifact: coordinate.clone(),
                        message: format!(
                            "recording {}: {error}",
                            sha_path(&classes_path).display()
                        ),
                    }
                })?;
            }
            std::fs::rename(&part_path, &classes_path).map_err(|error| ResolveError::Archive {
                artifact: coordinate.clone(),
                message: format!("finishing {}: {error}", classes_path.display()),
            })?;
            Ok(())
        })();
        if let Err(error) = outcome {
            let _ = std::fs::remove_file(&part_path);
            return Err(error);
        }
        Ok(classes_path)
    }

    fn fetch_from_repos(&self, rel: &str, cache_file: &Path) -> Result<(), ResolveError> {
        let mut first_failure: Option<String> = None;
        for repo in &self.repos {
            match download_into(repo, rel, cache_file) {
                Ok(()) => return Ok(()),
                Err(FetchError::Miss) => {}
                Err(FetchError::Fail(message)) => {
                    first_failure.get_or_insert(message);
                }
            }
        }
        Err(match first_failure {
            Some(message) => ResolveError::Fetch {
                artifact: rel.to_owned(),
                message,
            },
            None => ResolveError::NotFound {
                artifact: rel.to_owned(),
            },
        })
    }
}

impl Default for Resolver {
    /// The default resolver consults Google Maven then Maven Central.
    fn default() -> Resolver {
        Resolver::new(vec![MavenRepo::Google, MavenRepo::Central], None)
    }
}

enum FetchError {
    Miss,
    Fail(String),
}

/// The largest artifact a download accepts, in bytes: a guard against a
/// repository streaming garbage instead of an artifact (ureq's own
/// `read_to_vec` cap no longer applies — bodies stream to disk).
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Downloads `rel` from `repo` into `cache_file`, streaming the body
/// through a temporary `<cache_file>.part` while its SHA-256 is hashed
/// incrementally, then atomically renaming the file into place and
/// recording the digest in a sibling `<cache_file>.sha256`. A failed or
/// truncated download removes the partial file, so a broken artifact can
/// never be mistaken for a cached one.
///
/// A `file://` or plain-path repository streams a local file; a real
/// scheme (the Maven repos) is fetched over HTTP.
fn download_into(repo: &MavenRepo, rel: &str, cache_file: &Path) -> Result<(), FetchError> {
    let url = repo.url_for(rel);
    if !url.contains("://") || url.starts_with("file://") {
        let path = url.strip_prefix("file://").unwrap_or(&url);
        let source = match std::fs::File::open(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FetchError::Miss);
            }
            Err(error) => return Err(FetchError::Fail(error.to_string())),
        };
        return stream_to(&mut BufReader::new(source), cache_file, None).map_err(FetchError::Fail);
    }
    let response = match ureq::get(&url).call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Err(FetchError::Miss),
        Err(ureq::Error::StatusCode(code)) => return Err(FetchError::Fail(format!("HTTP {code}"))),
        Err(other) => return Err(FetchError::Fail(other.to_string())),
    };
    let mut body = response.into_body().into_reader();
    stream_to(&mut body, cache_file, Some(MAX_ARTIFACT_BYTES)).map_err(FetchError::Fail)
}

/// Streams `source` into `cache_file`, hashing as it goes: the body is
/// written to a temporary `<cache_file>.part`, the digest is recorded in
/// a sibling `<cache_file>.sha256`, and only then is the partial file
/// renamed over `cache_file` (atomic within the cache directory). Any
/// failure — including a body larger than `cap` bytes, when given —
/// removes the partial file and reports an error, leaving the cache
/// exactly as it was.
fn stream_to(source: &mut dyn Read, cache_file: &Path, cap: Option<u64>) -> Result<(), String> {
    let part = PathBuf::from(format!("{}.part", cache_file.display()));
    let outcome = (|| -> Result<(), String> {
        if let Some(parent) = cache_file.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return Err(format!("creating cache directory '{}'", parent.display()));
        }
        let mut file = std::fs::File::create(&part)
            .map_err(|error| format!("creating '{}': {error}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(|error| format!("reading download: {error}"))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if let Some(cap) = cap
                && total > cap
            {
                return Err(format!("artifact exceeds the {cap} byte limit"));
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|error| format!("writing '{}': {error}", part.display()))?;
        }
        drop(file);
        let digest = hex(&hasher.finalize());
        std::fs::write(sha_path(cache_file), digest)
            .map_err(|error| format!("recording '{}': {error}", sha_path(cache_file).display()))?;
        std::fs::rename(&part, cache_file)
            .map_err(|error| format!("moving '{}': {error}", part.display()))?;
        Ok(())
    })();
    if let Err(message) = outcome {
        let _ = std::fs::remove_file(&part);
        return Err(message);
    }
    Ok(())
}

/// Returns whether the cached artifact at `path` is intact: it exists
/// and its content hashes to the digest recorded in the sibling
/// `<path>.sha256`. The file is read in chunks, so verifying a large
/// cached jar never loads it into memory.
fn verified_cached(path: &Path) -> bool {
    let recorded = match std::fs::read_to_string(sha_path(path)) {
        Ok(recorded) => recorded,
        Err(_) => return false,
    };
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    recorded.trim() == hex(&hasher.finalize())
}

fn sha_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", path.display()))
}

/// Returns the digest recorded in the sibling `<path>.sha256` file, when
/// one is present. Used to tie an extracted `classes.jar` to the exact AAR
/// bytes it was derived from.
fn read_recorded_digest(path: &Path) -> Option<String> {
    let digest = std::fs::read_to_string(sha_path(path))
        .ok()?
        .trim()
        .to_owned();
    (!digest.is_empty()).then_some(digest)
}

/// Distinguishes concurrent extractions of the same coordinate within one
/// process, so their `.part` writes never share a file.
static PART_NONCE: AtomicU64 = AtomicU64::new(0);

fn default_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".cache")
            .join("uliab")
            .join("modules"),
        None => PathBuf::from(".cache").join("uliab").join("modules"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compares two Maven versions under Maven's ordering.
///
/// Versions are split on `.` and `-` (plus `_` and `+`, which Maven treats
/// as separators) into numeric and string segments; numeric segments
/// compare by value and string segments by qualifier rank, where the known
/// qualifiers order `alpha < beta < milestone < rc < snapshot < release <
/// sp`, unknown qualifiers sort above the known set and then
/// lexicographically, and a numeric segment outranks any qualifier. Missing
/// trailing segments compare as the release: `"1.0" == "1.0.0"`,
/// `"1.0-snapshot" < "1.0"`, `"2.10.0" > "2.1.0"`.
pub fn compare_maven_versions(a: &str, b: &str) -> Ordering {
    compare_tokens(&split_tokens(a), &split_tokens(b))
}

#[derive(Debug, PartialEq, Eq)]
enum VersionToken {
    Num(i64),
    Str(String),
}

fn split_tokens(version: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    for part in version.replace(['_', '+'], "-").split(['.', '-']) {
        if part.is_empty() {
            continue;
        }
        let mut alpha = String::new();
        let mut number = String::new();
        let mut numeric = None;
        for character in part.chars() {
            let is_numeric = character.is_ascii_digit();
            match (numeric, is_numeric) {
                (Some(true), false) => {
                    tokens.push(VersionToken::Num(number.parse().unwrap_or(i64::MAX)));
                    number.clear();
                }
                (Some(false), true) => {
                    tokens.push(VersionToken::Str(std::mem::take(&mut alpha)));
                }
                _ => {}
            }
            if is_numeric {
                number.push(character);
            } else {
                alpha.push(character);
            }
            numeric = Some(is_numeric);
        }
        if !number.is_empty() {
            tokens.push(VersionToken::Num(number.parse().unwrap_or(i64::MAX)));
        }
        if !alpha.is_empty() {
            tokens.push(VersionToken::Str(alpha));
        }
    }
    tokens
}

fn compare_tokens(a: &[VersionToken], b: &[VersionToken]) -> Ordering {
    let length = a.len().max(b.len());
    for index in 0..length {
        let ordering = match (a.get(index), b.get(index)) {
            (Some(left), Some(right)) => compare_token(left, right),
            (Some(left), None) => compare_with_release(left),
            (None, Some(right)) => compare_with_release(right).reverse(),
            (None, None) => Ordering::Equal,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_token(left: &VersionToken, right: &VersionToken) -> Ordering {
    match (left, right) {
        (VersionToken::Num(a), VersionToken::Num(b)) => a.cmp(b),
        (VersionToken::Num(_), VersionToken::Str(_)) => Ordering::Greater,
        (VersionToken::Str(_), VersionToken::Num(_)) => Ordering::Less,
        (VersionToken::Str(a), VersionToken::Str(b)) => {
            match (qualifier_rank(a), qualifier_rank(b)) {
                (Some(rank_a), Some(rank_b)) => rank_a.cmp(&rank_b).then_with(|| a.cmp(b)),
                // An unknown qualifier sorts above every known one — the
                // same rule a qualifier compares by against the implicit
                // release padding.
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        }
    }
}

/// A token compared against a missing (release-padded) counterpart.
fn compare_with_release(token: &VersionToken) -> Ordering {
    match token {
        VersionToken::Num(value) => value.cmp(&0),
        VersionToken::Str(qualifier) => match qualifier_rank(qualifier) {
            Some(rank) => rank.cmp(&RELEASE_RANK),
            None => Ordering::Greater,
        },
    }
}

const RELEASE_RANK: i32 = 5;

/// Ranks the known Maven qualifiers, lower ranking being older. Unknown
/// qualifiers return `None` and sort above the known set.
fn qualifier_rank(qualifier: &str) -> Option<i32> {
    match qualifier.to_ascii_lowercase().as_str() {
        "alpha" | "a" => Some(0),
        "beta" | "b" => Some(1),
        "milestone" | "m" => Some(2),
        "rc" | "cr" => Some(3),
        "snapshot" => Some(4),
        "" | "final" | "release" | "ga" => Some(RELEASE_RANK),
        "sp" => Some(6),
        _ => None,
    }
}

/// The dependency edges parsed from a POM.
#[derive(Debug)]
struct PomDependency {
    group: String,
    artifact: String,
    version: Option<String>,
    scope: PomScope,
    optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PomScope {
    Compile,
    Runtime,
    /// `test`, `provided`, or `system`: not part of a consumer's graph.
    Skip,
}

#[derive(Debug)]
struct PomProject {
    packaging: String,
    deps: Vec<PomDependency>,
    managed_deps: Vec<PomDependency>,
}

/// Normalizes a version as read from a POM `<version>` element into the
/// concrete version a coordinate resolves to.
///
/// A dependency declared with a Maven hard pin (`<version>[1.2.3]</version>`)
/// pins the exact version — the brackets are pin syntax, not part of the
/// version. Such a dependency resolves to the same artifact as the
/// unpinned `1.2.3`, so the brackets are stripped here. Inclusive range
/// pins (which carry a comma, e.g. `[1.0,2.0]`) are left untouched: a
/// range is not a single concrete version, and failing to resolve it is
/// the honest outcome.
fn normalize_pom_version(version: &str) -> String {
    if version.len() > 2
        && version.starts_with("[")
        && version.ends_with("]")
        && !version.contains(",")
    {
        version[1..version.len() - 1].to_owned()
    } else {
        version.to_owned()
    }
}

/// The version placeholder Gradle emits in a dependency when the version
/// is expected to come from a `dependencyManagement` section.
const UNSPECIFIED_VERSION: &str = "unspecified";

/// Parses a POM's packaging, its direct `compile`/`runtime`-scoped
/// dependencies, and its `dependencyManagement` entries.
///
/// `test`/`provided`/`system` and `optional` dependencies are dropped from
/// the direct dependencies list, as are dependencies that are not part of a
/// consumer's graph. Managed dependencies are preserved regardless of scope
/// since they supply version constraints to consumers.
fn parse_pom(bytes: &[u8]) -> Result<PomProject, String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();

    let mut stack: Vec<String> = Vec::new();
    let mut packaging = "jar".to_owned();
    let mut in_management = false;
    let mut in_management_deps = false;
    let mut in_dependencies = false;
    let mut current: Option<PomDependency> = None;
    let mut field: Option<String> = None;
    let mut deps = Vec::new();
    let mut managed_deps = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(element)) => {
                let name = element.name().as_ref().to_vec();
                let name = String::from_utf8_lossy(&name).into_owned();
                let parent = stack.last().cloned();
                stack.push(name.clone());
                field = None;
                match name.as_str() {
                    "packaging" if parent.as_deref() == Some("project") => field = Some(name),
                    "dependencyManagement" => in_management = true,
                    "dependencies"
                        if in_management && parent.as_deref() == Some("dependencyManagement") =>
                    {
                        in_management_deps = true;
                    }
                    "dependencies" if !in_management && parent.as_deref() == Some("project") => {
                        in_dependencies = true;
                    }
                    "dependency"
                        if (in_dependencies || in_management_deps)
                            && parent.as_deref() == Some("dependencies")
                            && current.is_none() =>
                    {
                        current = Some(PomDependency {
                            group: String::new(),
                            artifact: String::new(),
                            version: None,
                            scope: PomScope::Compile,
                            optional: false,
                        });
                    }
                    "groupId" | "artifactId" | "version" | "scope" | "optional"
                        if current.is_some() && parent.as_deref() == Some("dependency") =>
                    {
                        field = Some(name);
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Text(text)) => {
                let content = text
                    .unescape()
                    .map_err(|error| format!("decoding text: {error}"))?
                    .into_owned();
                if content.trim().is_empty() {
                    continue;
                }
                let field_name = field.as_deref();
                let Some(dependency) = current.as_mut() else {
                    if field_name == Some("packaging") {
                        packaging = content;
                    }
                    continue;
                };
                match field_name {
                    Some("groupId") => dependency.group = content,
                    Some("artifactId") => dependency.artifact = content,
                    Some("version") => {
                        let version = normalize_pom_version(&content);
                        // A dependency whose version is `unspecified` gets its
                        // version from the POM's `dependencyManagement`. Treat
                        // it as version-less so the resolver fills it in from
                        // managed versions, rather than trying to resolve a
                        // literal artifact named `unspecified`. Managed
                        // dependency entries keep their version: a BOM may
                        // itself declare `unspecified` for a dep the consumer
                        // pins, and preserving that here lets it win.
                        dependency.version = if in_dependencies && version == UNSPECIFIED_VERSION {
                            None
                        } else {
                            Some(version)
                        };
                    }
                    Some("scope") => {
                        dependency.scope = match content.as_str() {
                            "runtime" => PomScope::Runtime,
                            "test" | "provided" | "system" => PomScope::Skip,
                            _ => PomScope::Compile,
                        };
                    }
                    Some("optional") => dependency.optional = content == "true",
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::End(element)) => {
                let name = element.name().as_ref().to_vec();
                let name = String::from_utf8_lossy(&name).into_owned();
                if field.as_deref() == Some(name.as_str()) {
                    field = None;
                }
                stack.pop();
                match name.as_str() {
                    "dependency" => {
                        if let Some(dependency) = current.take()
                            && !dependency.group.is_empty()
                            && !dependency.artifact.is_empty()
                        {
                            if in_management_deps {
                                managed_deps.push(dependency);
                            } else if !dependency.optional && dependency.scope != PomScope::Skip {
                                deps.push(dependency);
                            }
                        }
                    }
                    "dependencies" => {
                        if in_management_deps {
                            in_management_deps = false;
                        } else {
                            in_dependencies = false;
                        }
                    }
                    "dependencyManagement" => in_management = false,
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("reading XML: {error}")),
        }
    }
    Ok(PomProject {
        packaging,
        deps,
        managed_deps,
    })
}

/// A node in the expanded dependency graph: one concrete version of one
/// `group:artifact`.
#[derive(Debug)]
struct GraphNode {
    packaging: String,
    edges: Vec<PomEdge>,
}

/// A resolved compile/runtime-scoped child edge.
#[derive(Debug)]
struct PomEdge {
    group: String,
    artifact: String,
    scope: PomScope,
}

struct Session<'a> {
    resolver: &'a Resolver,
    graph: BTreeMap<(String, String), BTreeMap<String, GraphNode>>,
    order: Vec<(String, String, String)>,
    seen: BTreeSet<(String, String, String)>,
    notes: Vec<String>,
    managed_versions: BTreeMap<(String, String), String>,
}

impl<'a> Session<'a> {
    fn new(resolver: &'a Resolver) -> Session<'a> {
        Session {
            resolver,
            graph: BTreeMap::new(),
            order: Vec::new(),
            seen: BTreeSet::new(),
            notes: Vec::new(),
            managed_versions: BTreeMap::new(),
        }
    }

    /// Fetches the POM for `group:artifact:version`, parses it, and records
    /// its node plus the POM of every resolvable child, recursively.
    ///
    /// When the POM's packaging is `pom` (a BOM), its `dependencyManagement`
    /// entries are recorded as version constraints. Version-less child
    /// dependencies are resolved against these constraints.
    fn expand(&mut self, group: &str, artifact: &str, version: &str) -> Result<(), ResolveError> {
        let key = (group.to_owned(), artifact.to_owned(), version.to_owned());
        if self.seen.contains(&key) {
            return Ok(());
        }
        self.seen.insert(key.clone());

        let pom_path = self
            .resolver
            .fetch_cached(group, artifact, version, "pom")?;
        let pom_bytes = std::fs::read(&pom_path).map_err(|error| ResolveError::Fetch {
            artifact: format!("{group}:{artifact}:{version}"),
            message: format!("reading cached POM: {error}"),
        })?;
        let pom = parse_pom(&pom_bytes).map_err(|message| ResolveError::Parser {
            artifact: format!("{group}:{artifact}:{version}"),
            message,
        })?;

        // BOMs (packaging = "pom") contribute version constraints from
        // their `dependencyManagement` section. The first BOM to declare
        // a constraint for a given `group:artifact` wins (nearest
        // definition wins, matching Maven semantics).
        if pom.packaging == "pom" {
            for managed in &pom.managed_deps {
                if let Some(ref ver) = managed.version {
                    self.managed_versions
                        .entry((managed.group.clone(), managed.artifact.clone()))
                        .or_insert_with(|| ver.clone());
                }
            }
        }

        // Every POM's own `dependencyManagement` constraints resolve its
        // version-less deps too. This matters for aar POMs, which are not
        // BOMs and therefore never contribute to `managed_versions` above,
        // but may still pin a dependency (e.g. `kotlin-stdlib`) in their
        // own `dependencyManagement` with the dependency using the Gradle
        // `unspecified` placeholder.
        let mut local_managed: BTreeMap<(String, String), String> = BTreeMap::new();
        for managed in &pom.managed_deps {
            if let Some(ref ver) = managed.version {
                local_managed
                    .entry((managed.group.clone(), managed.artifact.clone()))
                    .or_insert_with(|| ver.clone());
            }
        }

        let mut node = GraphNode {
            packaging: pom.packaging,
            edges: Vec::new(),
        };
        for dependency in pom.deps {
            let version = match dependency.version {
                None => {
                    // Version-less child: look up in the POM's own
                    // `dependencyManagement`, falling back to BOM-managed
                    // versions.
                    if let Some(local) =
                        local_managed.get(&(dependency.group.clone(), dependency.artifact.clone()))
                    {
                        local.clone()
                    } else if let Some(managed) = self
                        .managed_versions
                        .get(&(dependency.group.clone(), dependency.artifact.clone()))
                    {
                        managed.clone()
                    } else {
                        self.notes.push(format!(
                            "{}:{} declares no version and no BOM provides one; \
                             its dependencies are not followed",
                            dependency.group, dependency.artifact
                        ));
                        continue;
                    }
                }
                Some(ref v) if v.contains("${") => {
                    // Property version: check managed versions before skipping.
                    if let Some(managed) = self
                        .managed_versions
                        .get(&(dependency.group.clone(), dependency.artifact.clone()))
                    {
                        managed.clone()
                    } else {
                        self.notes.push(format!(
                            "{}:{}:{} uses a property version; parent POMs are \
                             outside the resolver's scope",
                            dependency.group, dependency.artifact, v
                        ));
                        continue;
                    }
                }
                Some(v) => v,
            };
            self.expand(&dependency.group, &dependency.artifact, &version)?;
            node.edges.push(PomEdge {
                group: dependency.group,
                artifact: dependency.artifact,
                scope: dependency.scope,
            });
        }

        self.graph
            .entry((group.to_owned(), artifact.to_owned()))
            .or_default()
            .insert(version.to_owned(), node);
        self.order.push(key);
        Ok(())
    }

    /// Picks the winning version per `group:artifact` by the
    /// highest-version rule, recording a note for each loser. Ties keep the
    /// first-discovered version.
    fn winners(&mut self) -> BTreeMap<(String, String), String> {
        let mut winners = BTreeMap::new();
        for (group, artifact) in self.graph.keys() {
            let mut chosen: Option<&str> = None;
            for (order_group, order_artifact, version) in &self.order {
                if order_group != group || order_artifact != artifact {
                    continue;
                }
                chosen = Some(match chosen {
                    None => version,
                    Some(current)
                        if compare_maven_versions(version, current) == Ordering::Greater =>
                    {
                        version
                    }
                    Some(current) => current,
                });
            }
            let chosen = chosen.expect("every group in the graph was discovered in order");
            winners.insert((group.clone(), artifact.clone()), chosen.to_owned());
            for (order_group, order_artifact, version) in &self.order {
                if order_group == group && order_artifact == artifact && version != chosen {
                    self.notes.push(format!(
                        "{order_group}:{order_artifact}:{version} superseded by {chosen}"
                    ));
                }
            }
        }
        winners
    }

    fn winner_node(
        &self,
        winners: &BTreeMap<(String, String), String>,
        group: &str,
        artifact: &str,
    ) -> Option<&GraphNode> {
        let version = winners.get(&(group.to_owned(), artifact.to_owned()))?;
        self.graph
            .get(&(group.to_owned(), artifact.to_owned()))?
            .get(version)
    }

    /// Builds the classpath buckets from the winning nodes, along with the
    /// resolved path of each direct declared root coordinate that
    /// materialized to a jar or an AAR `classes.jar`. The root map is
    /// derived from the same materializations that build the buckets (the
    /// union of every bucket's coordinate-keyed paths), so no artifact is
    /// fetched a second time.
    fn classpath(
        &mut self,
        declared: &[DeclaredDep],
        winners: &BTreeMap<(String, String), String>,
    ) -> Result<(Classpath, RootPaths), ResolveError> {
        let roots = |scopes: &[MavenScope]| -> Vec<(String, String)> {
            declared
                .iter()
                .filter(|dep| scopes.contains(&dep.scope))
                .map(|dep| {
                    (
                        dep.dependency.group.clone(),
                        dep.dependency.artifact.clone(),
                    )
                })
                .collect()
        };

        let api_roots = roots(&[MavenScope::Api]);
        let api_direct: BTreeMap<(String, String), (String, String)> = api_roots
            .iter()
            .filter_map(|(group, artifact)| {
                let version = winners.get(&(group.clone(), artifact.clone()))?.clone();
                let packaging = self
                    .winner_node(winners, group, artifact)
                    .map(|node| node.packaging.clone())
                    .unwrap_or_else(|| "jar".to_owned());
                Some(((group.clone(), artifact.clone()), (version, packaging)))
            })
            .collect();

        let compile_only = roots(&[MavenScope::CompileOnly]);
        let mut compile = self.bucket(
            winners,
            &roots(&[MavenScope::Api, MavenScope::Implementation]),
            Some(PomScope::Compile),
        );
        for (group, artifact) in &compile_only {
            let Some(version) = winners.get(&(group.clone(), artifact.clone())) else {
                continue;
            };
            let packaging = self
                .winner_node(winners, group, artifact)
                .map(|node| node.packaging.clone())
                .unwrap_or_else(|| "jar".to_owned());
            compile.insert(
                (group.clone(), artifact.clone()),
                (version.clone(), packaging),
            );
        }

        let test_roots = roots(&[MavenScope::TestImplementation]);
        let android_test_roots = roots(&[MavenScope::AndroidTestImplementation]);
        let runtime = self.bucket(
            winners,
            &roots(&[
                MavenScope::Api,
                MavenScope::Implementation,
                MavenScope::RuntimeOnly,
            ]),
            None,
        );
        let processor = self.bucket(winners, &roots(&[MavenScope::Ksp]), None);
        let test_compile = self.bucket(winners, &test_roots, Some(PomScope::Compile));
        let test_runtime = self.bucket(winners, &test_roots, None);
        let android_test_compile =
            self.bucket(winners, &android_test_roots, Some(PomScope::Compile));
        let android_test_runtime = self.bucket(winners, &android_test_roots, None);

        let compile_paths = self.materialize(&compile)?;
        let runtime_paths = self.materialize(&runtime)?;
        let processor_paths = self.materialize(&processor)?;
        let api_paths = self.materialize(&api_direct)?;
        let test_compile_paths = self.materialize(&merge(compile.clone(), test_compile))?;
        let test_runtime_paths = self.materialize(&merge(runtime.clone(), test_runtime))?;
        let android_test_compile_paths = self.materialize(&merge(compile, android_test_compile))?;
        let android_test_runtime_paths = self.materialize(&merge(runtime, android_test_runtime))?;

        let mut root_paths: RootPaths = BTreeMap::new();
        for bucket_paths in [
            &compile_paths,
            &runtime_paths,
            &processor_paths,
            &api_paths,
            &test_compile_paths,
            &test_runtime_paths,
            &android_test_compile_paths,
            &android_test_runtime_paths,
        ] {
            for (key, path) in bucket_paths {
                let (group, artifact) = key;
                let declared_root = declared.iter().any(|dep| {
                    dep.dependency.group == *group && dep.dependency.artifact == *artifact
                });
                if declared_root {
                    root_paths
                        .entry((group.clone(), artifact.clone()))
                        .or_insert(path.clone());
                }
            }
        }

        Ok((
            Classpath {
                compile: path_entries(&compile_paths),
                runtime: path_entries(&runtime_paths),
                processor: path_entries(&processor_paths),
                api: path_entries(&api_paths),
                test_compile: path_entries(&test_compile_paths),
                test_runtime: path_entries(&test_runtime_paths),
                android_test_compile: path_entries(&android_test_compile_paths),
                android_test_runtime: path_entries(&android_test_runtime_paths),
                compose_runtimes: Vec::new(),
            },
            root_paths,
        ))
    }

    /// The reachable winner nodes from `roots`, following only
    /// `edge_scope` edges when given (compile classpaths) or all edges when
    /// `None` (runtime classpaths).
    fn bucket(
        &self,
        winners: &BTreeMap<(String, String), String>,
        roots: &[(String, String)],
        edge_scope: Option<PomScope>,
    ) -> BTreeMap<(String, String), (String, String)> {
        let mut reachable = BTreeSet::new();
        let mut queue: VecDeque<(String, String)> = roots.iter().cloned().collect();
        while let Some((group, artifact)) = queue.pop_front() {
            if !reachable.insert((group.clone(), artifact.clone())) {
                continue;
            }
            let Some(node) = self.winner_node(winners, &group, &artifact) else {
                continue;
            };
            for edge in &node.edges {
                if edge_scope.is_none_or(|scope| edge.scope == scope) {
                    queue.push_back((edge.group.clone(), edge.artifact.clone()));
                }
            }
        }
        reachable
            .into_iter()
            .filter_map(|(group, artifact)| {
                let version = winners.get(&(group.clone(), artifact.clone()))?.clone();
                let packaging = self
                    .winner_node(winners, &group, &artifact)
                    .map(|node| node.packaging.clone())
                    .unwrap_or_else(|| "jar".to_owned());
                Some(((group, artifact), (version, packaging)))
            })
            .collect()
    }

    /// Downloads and materializes every node in `set` on a worker pool,
    /// returning their classpath paths keyed by (`group`, `artifact`) in
    /// deterministic order.  Plain jars contribute their own file; Android
    /// AARs contribute the `classes.jar` extracted from the archive; other
    /// packaging is noted and contributes nothing.  The first failure
    /// aborts the remaining fetches and surfaces its error.
    fn materialize(
        &mut self,
        set: &BTreeMap<(String, String), (String, String)>,
    ) -> Result<RootPaths, ResolveError> {
        let artifacts: Vec<((String, String), (String, String))> = set
            .iter()
            .filter_map(|((group, artifact), (version, packaging))| {
                if packaging == "jar" || packaging == "aar" {
                    Some((
                        (group.clone(), artifact.clone()),
                        (version.clone(), packaging.clone()),
                    ))
                } else {
                    None
                }
            })
            .collect();
        for ((group, artifact), (version, packaging)) in set {
            if packaging != "pom" && packaging != "jar" && packaging != "aar" {
                self.notes.push(format!(
                    "{group}:{artifact}:{version} uses packaging '{packaging}'; only jar \
                     and aar artifacts are materialized"
                ));
            }
        }
        let workers = std::thread::available_parallelism()
            .map_or(4, |count| count.get().min(8))
            .min(artifacts.len())
            .max(1);
        let queue = Mutex::new(VecDeque::from_iter(0..artifacts.len()));
        let results = Mutex::new(vec![None; artifacts.len()]);
        let failed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let resolver = &self.resolver;
                let artifacts = &artifacts;
                let queue = &queue;
                let results = &results;
                let failed = &failed;
                scope.spawn(move || {
                    loop {
                        if failed.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        let index = queue
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .pop_front();
                        let Some(index) = index else { break };
                        let ((group, artifact), (version, packaging)) = &artifacts[index];
                        let outcome = if packaging == "aar" {
                            resolver.materialize_aar(group, artifact, version)
                        } else {
                            resolver.fetch_cached(group, artifact, version, "jar")
                        };
                        if outcome.is_err() {
                            failed.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        results
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())[index] =
                            Some(outcome);
                    }
                });
            }
        });
        let results = results
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut paths: RootPaths = BTreeMap::new();
        for (result, ((group, artifact), _)) in results.iter().zip(artifacts.iter()) {
            match result {
                Some(Ok(path)) => {
                    paths.insert((group.clone(), artifact.clone()), path.clone());
                }
                Some(Err(error)) => return Err(error.clone()),
                None => {}
            }
        }
        Ok(paths)
    }
}

/// The materialized paths of `paths`, in the map's deterministic
/// coordinate-sorted order, as owned values.
fn path_entries(paths: &RootPaths) -> Vec<PathBuf> {
    paths.values().cloned().collect()
}

fn merge(
    base: BTreeMap<(String, String), (String, String)>,
    extra: BTreeMap<(String, String), (String, String)>,
) -> BTreeMap<(String, String), (String, String)> {
    let mut merged = base;
    for (key, value) in extra {
        merged.entry(key).or_insert(value);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, Write};
    use std::sync::Arc;

    fn repo_pom(
        group: &str,
        artifact: &str,
        version: &str,
        children: &[(&str, &str, &str)],
    ) -> String {
        let mut pom = format!(
            "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion>\
             <groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>{version}</version>"
        );
        if !children.is_empty() {
            pom.push_str("<dependencies>");
            for (child, child_version, scope) in children {
                let child_group = child.split(':').next().unwrap();
                let child_artifact = child.split(':').nth(1).unwrap();
                let scope_tag = if scope.is_empty() {
                    String::new()
                } else {
                    format!("<scope>{scope}</scope>")
                };
                pom.push_str(&format!(
                    "<dependency><groupId>{child_group}</groupId>\
                     <artifactId>{child_artifact}</artifactId><version>{child_version}</version>{scope_tag}</dependency>"
                ));
            }
            pom.push_str("</dependencies>");
        }
        pom.push_str("</project>");
        pom
    }

    fn write_artifact(root: &Path, group: &str, artifact: &str, version: &str, pom: &str) {
        let rel = format!(
            "{}/{}/{}/{}-{}",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version
        );
        std::fs::create_dir_all(root.join(&rel)).unwrap();
        std::fs::write(root.join(format!("{rel}.pom")), pom).unwrap();
        std::fs::write(
            root.join(format!("{rel}.jar")),
            format!("{artifact}-{version}"),
        )
        .unwrap();
    }

    /// Writes a real Android AAR (a zip holding a `classes.jar`) at the
    /// coordinate's location in `root`, so the resolver's aar path can be
    /// exercised against an actual archive rather than a placeholder.
    fn write_aar(root: &Path, group: &str, artifact: &str, version: &str) -> PathBuf {
        write_aar_with(root, group, artifact, version, b"fake classes")
    }

    /// Like [`write_aar`], but with a caller-chosen `classes.jar` payload so
    /// tests can serve different bytes for the same coordinate.
    fn write_aar_with(
        root: &Path,
        group: &str,
        artifact: &str,
        version: &str,
        content: &[u8],
    ) -> PathBuf {
        let dir = root.join(format!(
            "{}/{}/{}/",
            group.replace('.', "/"),
            artifact,
            version
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let aar_path = dir.join(format!("{artifact}-{version}.aar"));
        let file = std::fs::File::create(&aar_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("classes.jar", options).unwrap();
        writer.write_all(content).unwrap();
        writer.finish().unwrap();
        aar_path
    }

    struct LocalRepo {
        root: PathBuf,
    }

    impl LocalRepo {
        fn new() -> LocalRepo {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "uliab-maven-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            LocalRepo { root }
        }

        fn add(&self, group: &str, artifact: &str, version: &str, children: &[(&str, &str, &str)]) {
            write_artifact(
                &self.root,
                group,
                artifact,
                version,
                &repo_pom(group, artifact, version, children),
            );
        }

        fn resolver(&self) -> Resolver {
            Resolver::new(
                vec![MavenRepo::Custom(self.root.display().to_string())],
                Some(self.root.join("cache")),
            )
        }
    }

    fn declared(scope: MavenScope, coordinate: &str) -> DeclaredDep {
        DeclaredDep {
            scope,
            dependency: Dependency::parse(coordinate).unwrap(),
        }
    }

    fn jar_names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_coordinates() {
        let dependency = Dependency::parse("com.example:app:1.2.3").expect("parses");
        assert_eq!(dependency.group, "com.example");
        assert_eq!(dependency.artifact, "app");
        assert_eq!(dependency.version, "1.2.3");

        let managed = Dependency::parse("com.example:app").expect("version-less parses");
        assert_eq!(managed.group, "com.example");
        assert_eq!(managed.artifact, "app");
        assert!(managed.is_version_managed());

        assert!(Dependency::parse("com.example:app:1:extra").is_err());
        assert!(Dependency::parse("::").is_err());
        assert!(
            Dependency::parse("com.example:app:").is_err(),
            "empty version in three-part coord must be rejected"
        );
    }

    #[test]
    fn parses_a_deps_block() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "implementation".to_owned(),
            Value::Str("com.example:app:1.0".to_owned()),
        );
        entries.insert(
            "ksp".to_owned(),
            Value::List(vec![
                Value::Coordinate("com.example:proc:2.0".to_owned()),
                Value::Coordinate("com.example:proc2:2.0".to_owned()),
            ]),
        );
        entries.insert(
            "runtimeOnly".to_owned(),
            Value::Str("com.example:run:1.0".to_owned()),
        );
        let deps = parse_deps_block(&Value::Block(entries)).expect("parses");
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].scope, MavenScope::Implementation);
        assert_eq!(deps[0].dependency.artifact, "app");
        assert_eq!(deps[2].scope, MavenScope::Ksp);
        assert_eq!(deps[3].scope, MavenScope::RuntimeOnly);
        assert_eq!(deps[3].dependency.artifact, "run");
    }

    #[test]
    fn rejects_unknown_scope_and_bad_values() {
        let mut entries = BTreeMap::new();
        entries.insert("notAScope".to_owned(), Value::Str("a:b:1".to_owned()));
        assert!(parse_deps_block(&Value::Block(entries)).is_err());

        let mut entries = BTreeMap::new();
        entries.insert(
            "implementation".to_owned(),
            Value::Number(ulb_lang::token::Number::Int(1)),
        );
        assert!(parse_deps_block(&Value::Block(entries)).is_err());

        let mut entries = BTreeMap::new();
        entries.insert("implementation".to_owned(), Value::Str("a:b".to_owned()));
        let deps = parse_deps_block(&Value::Block(entries)).expect("version-less parses");
        assert_eq!(deps.len(), 1);
        assert!(deps[0].dependency.is_version_managed());
    }

    #[test]
    fn maven_version_ordering() {
        assert_eq!(compare_maven_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_maven_versions("1.0.1", "1.0"), Ordering::Greater);
        assert_eq!(compare_maven_versions("2.10.0", "2.1.0"), Ordering::Greater);
        assert_eq!(
            compare_maven_versions("1.0.0-rc1", "1.0.0-beta1"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-rc2", "1.0.0-rc1"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-snapshot", "1.0.0-rc1"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0", "1.0.0-snapshot"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-sp1", "1.0.0"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-alpha1", "1.0.0"),
            Ordering::Less
        );
        assert_eq!(
            compare_maven_versions("2.0.0", "2.0.0-alpha"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-1", "1.0.0"),
            Ordering::Greater
        );
        assert_eq!(compare_maven_versions("1.0.0", "1.0.0-1"), Ordering::Less);
        // Unknown qualifiers sort above every known qualifier and then
        // lexicographically among themselves.
        assert_eq!(
            compare_maven_versions("1.0.0-android", "1.0.0-final"),
            Ordering::Greater
        );
        assert_eq!(
            compare_maven_versions("1.0.0-beta", "1.0.0-custom"),
            Ordering::Less
        );
        assert_eq!(
            compare_maven_versions("1.0.0-zeta", "1.0.0-alpha"),
            Ordering::Greater
        );
    }

    #[test]
    fn parses_pom_compile_and_runtime_deps() {
        let pom = r#"<?xml version="1.0"?>
        <project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>com.example</groupId><artifactId>root</artifactId><version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId><artifactId>a</artifactId><version>1.0</version>
            </dependency>
            <dependency>
              <groupId>com.example</groupId><artifactId>b</artifactId><version>1.0</version>
              <scope>runtime</scope>
            </dependency>
            <dependency>
              <groupId>com.example</groupId><artifactId>c</artifactId><version>1.0</version>
              <scope>test</scope>
            </dependency>
            <dependency>
              <groupId>com.example</groupId><artifactId>d</artifactId><version>1.0</version>
              <optional>true</optional>
            </dependency>
          </dependencies>
        </project>"#;
        let parsed = parse_pom(pom.as_bytes()).expect("parses");
        assert_eq!(parsed.packaging, "jar");
        assert_eq!(parsed.deps.len(), 2);
        assert_eq!(parsed.deps[0].scope, PomScope::Compile);
        assert_eq!(parsed.deps[1].scope, PomScope::Runtime);
    }

    #[test]
    fn parses_pom_extracts_dependency_management() {
        let pom = r#"<project>
          <groupId>com.example</groupId><artifactId>root</artifactId><version>1.0</version>
          <dependencyManagement>
            <dependencies>
              <dependency>
                <groupId>com.example</groupId><artifactId>managed</artifactId><version>2.0</version>
              </dependency>
            </dependencies>
          </dependencyManagement>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId><artifactId>real</artifactId><version>1.0</version>
            </dependency>
          </dependencies>
        </project>"#;
        let parsed = parse_pom(pom.as_bytes()).expect("parses");
        assert_eq!(parsed.deps.len(), 1);
        assert_eq!(parsed.deps[0].artifact, "real");
        assert_eq!(parsed.managed_deps.len(), 1);
        assert_eq!(parsed.managed_deps[0].artifact, "managed");
        assert_eq!(parsed.managed_deps[0].version.as_deref(), Some("2.0"));
    }

    #[test]
    fn normalizes_hard_pin_versions() {
        let pom = r#"<project>
          <groupId>com.example</groupId><artifactId>root</artifactId><version>1.0</version>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId><artifactId>pinned</artifactId><version>[1.2.3]</version>
            </dependency>
            <dependency>
              <groupId>com.example</groupId><artifactId>ranged</artifactId><version>[1.0,2.0]</version>
            </dependency>
          </dependencies>
        </project>"#;
        let parsed = parse_pom(pom.as_bytes()).expect("parses");
        assert_eq!(parsed.deps.len(), 2);
        assert_eq!(
            parsed.deps[0].version.as_deref(),
            Some("1.2.3"),
            "a hard pin resolves to the pinned version"
        );
        assert_eq!(
            parsed.deps[1].version.as_deref(),
            Some("[1.0,2.0]"),
            "a range pin is not a single version and is left as-is"
        );
        assert_eq!(normalize_pom_version("[1.2.3]"), "1.2.3");
        assert_eq!(normalize_pom_version("1.2.3"), "1.2.3");
        assert_eq!(normalize_pom_version("${X}"), "${X}");
        assert_eq!(normalize_pom_version("[]"), "[]");
    }

    #[test]
    fn unspecified_in_dependency_management_is_preserved() {
        let pom = r#"<project>
          <groupId>com.example</groupId><artifactId>root</artifactId><version>1.0</version>
          <dependencyManagement>
            <dependencies>
              <dependency>
                <groupId>com.example</groupId><artifactId>managed</artifactId><version>unspecified</version>
              </dependency>
            </dependencies>
          </dependencyManagement>
          <dependencies>
            <dependency>
              <groupId>com.example</groupId><artifactId>real</artifactId><version>unspecified</version>
            </dependency>
          </dependencies>
        </project>"#;
        let parsed = parse_pom(pom.as_bytes()).expect("parses");
        assert_eq!(
            parsed.managed_deps[0].version.as_deref(),
            Some("unspecified"),
            "a managed entry keeps `unspecified` so an explicit consumer pin can win"
        );
        assert_eq!(
            parsed.deps[0].version.as_deref(),
            None,
            "a regular dependency's `unspecified` is treated as version-less"
        );
    }

    #[test]
    fn resolves_unspecified_version_from_same_pom_management() {
        let repo = LocalRepo::new();
        let pom = r#"<?xml version="1.0"?><project>
          <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>
          <dependencyManagement>
            <dependencies>
              <dependency>
                <groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>1.9.24</version>
              </dependency>
            </dependencies>
          </dependencyManagement>
          <dependencies>
            <dependency>
              <groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>unspecified</version><scope>runtime</scope>
            </dependency>
          </dependencies>
        </project>"#;
        write_artifact(&repo.root, "com.example", "lib", "1.0", pom);
        write_artifact(
            &repo.root,
            "org.jetbrains.kotlin",
            "kotlin-stdlib",
            "1.9.24",
            &repo_pom("org.jetbrains.kotlin", "kotlin-stdlib", "1.9.24", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:lib:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["lib-1.0.jar", "kotlin-stdlib-1.9.24.jar"],
            "the `unspecified` dep resolves from the POM's own dependencyManagement"
        );
    }

    #[test]
    fn versionless_dep_falls_back_to_bom_managed_versions() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "bom",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>two</artifactId><version>2.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "lib",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId><artifactId>two</artifactId><version>unspecified</version>
                </dependency>
              </dependencies>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "two",
            "2.0",
            &repo_pom("com.example", "two", "2.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom:1.0"),
                declared(MavenScope::Implementation, "com.example:lib:1.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["lib-1.0.jar", "two-2.0.jar"],
            "when the POM has no own management, the version-less dep uses a BOM-managed version"
        );
    }

    #[test]
    fn parses_pom_packaging() {
        let pom = "<project><packaging>aar</packaging></project>";
        assert_eq!(parse_pom(pom.as_bytes()).unwrap().packaging, "aar");
        assert_eq!(parse_pom(b"<project></project>").unwrap().packaging, "jar");
    }

    #[test]
    fn resolves_transitive_compile_classpath() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "one",
            "1.0",
            &[("com.example:two", "1.0", "")],
        );
        repo.add("com.example", "two", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["one-1.0.jar", "two-1.0.jar"]
        );
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["one-1.0.jar", "two-1.0.jar"]
        );
    }

    #[test]
    fn runtime_only_and_compile_only_scopes() {
        let repo = LocalRepo::new();
        repo.add("com.example", "a", "1.0", &[]);
        repo.add("com.example", "b", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::RuntimeOnly, "com.example:a:1.0"),
                declared(MavenScope::CompileOnly, "com.example:b:1.0"),
            ])
            .expect("resolves");
        assert_eq!(jar_names(&resolution.classpath.compile), vec!["b-1.0.jar"]);
        assert_eq!(jar_names(&resolution.classpath.runtime), vec!["a-1.0.jar"]);
    }

    #[test]
    fn compile_only_dep_does_not_leak_children() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "tool",
            "1.0",
            &[("com.example:helper", "1.0", "")],
        );
        repo.add("com.example", "helper", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::CompileOnly, "com.example:tool:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["tool-1.0.jar"]
        );
    }

    #[test]
    fn api_bucket_contains_only_direct_api_roots() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "api-lib",
            "1.0",
            &[("com.example:transitive", "1.0", "")],
        );
        repo.add("com.example", "transitive", "1.0", &[]);
        repo.add("com.example", "impl-lib", "2.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Api, "com.example:api-lib:1.0"),
                declared(MavenScope::Implementation, "com.example:impl-lib:2.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["api-lib-1.0.jar", "impl-lib-2.0.jar", "transitive-1.0.jar"]
        );
        assert_eq!(
            jar_names(&resolution.classpath.api),
            vec!["api-lib-1.0.jar"]
        );
    }

    #[test]
    fn api_bucket_empty_when_no_api_deps() {
        let repo = LocalRepo::new();
        repo.add("com.example", "lib", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:lib:1.0")])
            .expect("resolves");
        assert!(resolution.classpath.api.is_empty());
    }

    #[test]
    fn runtime_closure_includes_runtime_scope_edges() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "app",
            "1.0",
            &[("com.example:rt", "1.0", "runtime")],
        );
        repo.add("com.example", "rt", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:app:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["app-1.0.jar"]
        );
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["app-1.0.jar", "rt-1.0.jar"]
        );
    }

    #[test]
    fn conflict_resolves_to_highest_version() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "app",
            "1.0",
            &[("com.example:lib", "1.0", "")],
        );
        repo.add("com.example", "lib", "1.0", &[]);
        repo.add("com.example", "lib", "2.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:app:1.0"),
                declared(MavenScope::Implementation, "com.example:lib:2.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["app-1.0.jar", "lib-2.0.jar"]
        );
        let superseded = resolution
            .notes
            .iter()
            .find(|note| note.contains("superseded"));
        assert!(superseded.is_some(), "notes: {:?}", resolution.notes);
        assert!(superseded.unwrap().contains("lib:1.0"));
    }

    #[test]
    fn test_scope_layers_on_main_classpath() {
        let repo = LocalRepo::new();
        repo.add("com.example", "main", "1.0", &[]);
        repo.add("com.example", "t", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:main:1.0"),
                declared(MavenScope::TestImplementation, "com.example:t:1.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.test_compile),
            vec!["main-1.0.jar", "t-1.0.jar"]
        );
        assert_eq!(
            jar_names(&resolution.classpath.test_runtime),
            vec!["main-1.0.jar", "t-1.0.jar"]
        );
    }

    #[test]
    fn processor_scope_collects_ksp_deps() {
        let repo = LocalRepo::new();
        repo.add("com.example", "proc", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Ksp, "com.example:proc:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.processor),
            vec!["proc-1.0.jar"]
        );
        assert!(resolution.classpath.compile.is_empty());
    }

    #[test]
    fn pom_packaging_contributes_no_jar() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "bom",
            "1.0",
            &[("com.example:real", "1.0", "")],
        );
        repo.add("com.example", "real", "1.0", &[]);
        let bom_pom = repo_pom(
            "com.example",
            "bom",
            "1.0",
            &[("com.example:real", "1.0", "")],
        )
        .replace("<project>", "<project><packaging>pom</packaging>");
        write_artifact(&repo.root, "com.example", "bom", "1.0", &bom_pom);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:bom:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["real-1.0.jar"]
        );
    }

    #[test]
    fn aar_materialization_extracts_classes_jar() {
        let repo = LocalRepo::new();
        let group = "com.example";
        let artifact = "aarlib";
        let version = "1.0";
        let pom = repo_pom(group, artifact, version, &[])
            .replace("<project>", "<project><packaging>aar</packaging>");
        write_artifact(&repo.root, group, artifact, version, &pom);
        write_aar(&repo.root, group, artifact, version);
        let resolution = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:aarlib:1.0",
            )])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["aarlib-1.0-classes.jar"]
        );
        let joined = &resolution.classpath.compile[0];
        assert_eq!(std::fs::read(joined).unwrap(), b"fake classes");
        assert!(
            !resolution
                .notes
                .iter()
                .any(|note| note.contains("only jar and aar")),
            "aar packaging must not be flagged as unsupported: {:?}",
            resolution.notes
        );
    }

    #[test]
    fn aar_extracted_jar_path_is_stable_for_unchanged_coordinate() {
        let repo = LocalRepo::new();
        let group = "com.example";
        let artifact = "aarlib";
        let version = "1.0";
        let pom = repo_pom(group, artifact, version, &[])
            .replace("<project>", "<project><packaging>aar</packaging>");
        write_artifact(&repo.root, group, artifact, version, &pom);
        write_aar(&repo.root, group, artifact, version);
        let resolver = repo.resolver();
        let first = resolver
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:aarlib:1.0",
            )])
            .expect("resolves");
        let first_path = first.classpath.compile[0].clone();
        let second = resolver
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:aarlib:1.0",
            )])
            .expect("resolves");
        assert_eq!(second.classpath.compile, vec![first_path]);
    }

    #[test]
    fn aar_re_extracts_when_the_aar_changes() {
        // A mutable coordinate (like a SNAPSHOT) serves different bytes over
        // time. The extracted classes.jar must track the current AAR rather
        // than the first one seen, or a refetched AAR would silently leave
        // stale bytecode on the classpath.
        let repo = LocalRepo::new();
        let group = "com.example";
        let artifact = "aarlib";
        let version = "1.0-SNAPSHOT";
        let coordinate = format!("com.example:aarlib:{version}");
        let pom = repo_pom(group, artifact, version, &[])
            .replace("<project>", "<project><packaging>aar</packaging>");
        let resolver = repo.resolver();

        write_artifact(&repo.root, group, artifact, version, &pom);
        write_aar_with(&repo.root, group, artifact, version, b"first");
        let first = resolver
            .resolve(&[declared(MavenScope::Implementation, &coordinate)])
            .expect("resolves");
        let classes = first.classpath.compile[0].clone();
        assert_eq!(std::fs::read(&classes).unwrap(), b"first");

        write_aar_with(&repo.root, group, artifact, version, b"second");
        let aar_cache = classes.with_file_name(format!("{artifact}-{version}.aar"));
        let _ = std::fs::remove_file(&aar_cache);
        let _ = std::fs::remove_file(format!("{}.sha256", aar_cache.display()));

        let second = resolver
            .resolve(&[declared(MavenScope::Implementation, &coordinate)])
            .expect("resolves");
        assert_eq!(
            std::fs::read(&second.classpath.compile[0]).unwrap(),
            b"second",
            "a refetched AAR must invalidate the previously-extracted classes.jar"
        );
    }

    #[test]
    fn aar_that_is_not_a_zip_is_an_archive_error() {
        let repo = LocalRepo::new();
        let group = "com.example";
        let artifact = "badaar";
        let version = "1.0";
        let pom = repo_pom(group, artifact, version, &[])
            .replace("<project>", "<project><packaging>aar</packaging>");
        write_artifact(&repo.root, group, artifact, version, &pom);
        let dir = repo.root.join(format!(
            "{}/{}/{}/",
            group.replace('.', "/"),
            artifact,
            version
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{artifact}-{version}.aar")),
            b"this is not a zip archive",
        )
        .unwrap();
        let error = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:badaar:1.0",
            )])
            .expect_err("a non-zip aar must fail");
        assert!(
            matches!(error, ResolveError::Archive { .. }),
            "expected Archive error, got {error}"
        );
    }

    #[test]
    fn aar_missing_classes_jar_is_an_archive_error() {
        let repo = LocalRepo::new();
        let group = "com.example";
        let artifact = "badaar";
        let version = "1.0";
        let pom = repo_pom(group, artifact, version, &[])
            .replace("<project>", "<project><packaging>aar</packaging>");
        write_artifact(&repo.root, group, artifact, version, &pom);
        let dir = repo.root.join(format!(
            "{}/{}/{}/",
            group.replace('.', "/"),
            artifact,
            version
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let aar_path = dir.join(format!("{artifact}-{version}.aar"));
        let file = std::fs::File::create(&aar_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("not-classes.txt", options).unwrap();
        writer.write_all(b"nope").unwrap();
        writer.finish().unwrap();
        let error = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:badaar:1.0",
            )])
            .expect_err("aar without classes.jar must fail");
        assert!(
            matches!(error, ResolveError::Archive { .. }),
            "expected Archive error, got {error}"
        );
    }

    #[test]
    fn skips_property_versions_with_a_note() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "app",
            "1.0",
            &[("com.example:lib", "${lib.version}", "")],
        );
        repo.add("com.example", "lib", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:app:1.0")])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.runtime),
            vec!["app-1.0.jar"]
        );
        assert!(
            resolution
                .notes
                .iter()
                .any(|note| note.contains("property version")),
            "notes: {:?}",
            resolution.notes
        );
    }

    #[test]
    fn cache_serves_a_second_resolution() {
        let repo = LocalRepo::new();
        repo.add("com.example", "one", "1.0", &[]);
        let resolver = repo.resolver();
        let first = resolver
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("resolves");
        let jar_path = &first.classpath.compile[0];
        let pom_path = jar_path.with_extension("pom");
        assert!(pom_path.exists());
        assert!(sha_path(jar_path).exists());
        assert!(sha_path(&pom_path).exists());
        assert_ne!(sha_path(jar_path), sha_path(&pom_path));

        std::fs::remove_dir_all(repo.root.join("com")).unwrap();
        let cached = resolver
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("served from cache without the repository");
        assert_eq!(first.classpath.compile, cached.classpath.compile);
    }

    #[test]
    fn a_corrupted_cache_entry_is_refetched() {
        let repo = LocalRepo::new();
        repo.add("com.example", "one", "1.0", &[]);
        let resolver = repo.resolver();
        resolver
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("resolves");
        let jar_path = &resolver
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("resolves")
            .classpath
            .compile[0];
        std::fs::write(jar_path, b"corrupted").unwrap();
        resolver
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("refetches and recovers");
        assert_eq!(std::fs::read(jar_path).unwrap(), b"one-1.0");
    }

    /// A localhost HTTP repository that serves the Maven layout from a
    /// local root, used to exercise the streaming and parallel download
    /// paths (`file://` repos are too fast to observe concurrency). Every
    /// request is held open briefly while its connection is counted, so a
    /// test can assert how many fetches ran at once; with `truncate`, jar
    /// responses announce a longer body than they send, simulating a
    /// download that breaks mid-body.
    struct HttpRepo {
        url: String,
        root: PathBuf,
        max_concurrent: Arc<std::sync::atomic::AtomicUsize>,
        _thread: std::thread::JoinHandle<()>,
    }

    impl HttpRepo {
        fn new(truncate: bool) -> HttpRepo {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "uliab-maven-http-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let max_concurrent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = in_flight.clone();
            let high_water = max_concurrent.clone();
            let serve_root = root.clone();
            let thread = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let counter = counter.clone();
                    let high_water = high_water.clone();
                    let serve_root = serve_root.clone();
                    let truncate = truncate;
                    std::thread::spawn(move || {
                        let mut stream = BufReader::new(stream);
                        let mut request = String::new();
                        if stream.read_line(&mut request).is_err() {
                            return;
                        }
                        let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        let serving = stream.get_mut();
                        let file = serve_root.join(path.trim_start_matches('/'));
                        let active = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        high_water.fetch_max(active, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        let body = std::fs::read(&file).unwrap_or_default();
                        let truncated = truncate
                            && file.extension().is_some_and(|extension| extension == "jar");
                        if body.is_empty() {
                            let _ = serving.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        } else {
                            let length = if truncated {
                                body.len() + 100
                            } else {
                                body.len()
                            };
                            let _ = write!(
                                serving,
                                "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                            );
                            let _ = serving.write_all(&body);
                        }
                        let _ = serving.flush();
                        counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    });
                }
            });
            HttpRepo {
                url,
                root,
                max_concurrent,
                _thread: thread,
            }
        }

        fn add(&self, group: &str, artifact: &str, version: &str, children: &[(&str, &str, &str)]) {
            write_artifact(
                &self.root,
                group,
                artifact,
                version,
                &repo_pom(group, artifact, version, children),
            );
        }

        fn resolver(&self) -> Resolver {
            Resolver::new(
                vec![MavenRepo::Custom(self.url.clone())],
                Some(self.root.join("cache")),
            )
        }
    }

    fn part_files(root: &Path) -> Vec<PathBuf> {
        let mut parts = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".part"))
                {
                    parts.push(path);
                }
            }
        }
        parts
    }

    #[test]
    fn download_records_the_streamed_digest() {
        let repo = HttpRepo::new(false);
        repo.add("com.example", "one", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:one:1.0")])
            .expect("resolves");
        let jar_path = &resolution.classpath.compile[0];
        let bytes = std::fs::read(jar_path).unwrap();
        let digest = hex(&Sha256::digest(&bytes));
        let recorded = std::fs::read_to_string(sha_path(jar_path)).unwrap();
        assert_eq!(recorded.trim(), digest);
    }

    #[test]
    fn a_truncated_download_leaves_no_part_file() {
        let repo = HttpRepo::new(true);
        repo.add("com.example", "broken", "1.0", &[]);
        let error = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:broken:1.0",
            )])
            .expect_err("truncated jar fails resolution");
        assert!(matches!(error, ResolveError::Fetch { .. }), "{error}");
        assert!(
            part_files(&repo.root.join("cache")).is_empty(),
            "partial downloads must be removed"
        );
    }

    #[test]
    fn jar_downloads_run_in_parallel() {
        let repo = HttpRepo::new(false);
        for artifact in ["a", "b", "c", "d", "e", "f"] {
            repo.add("com.example", artifact, "1.0", &[]);
        }
        let deps: Vec<DeclaredDep> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|artifact| {
                declared(
                    MavenScope::Implementation,
                    &format!("com.example:{artifact}:1.0"),
                )
            })
            .collect();
        let resolution = repo.resolver().resolve(&deps).expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec![
                "a-1.0.jar",
                "b-1.0.jar",
                "c-1.0.jar",
                "d-1.0.jar",
                "e-1.0.jar",
                "f-1.0.jar"
            ]
        );
        assert!(
            repo.max_concurrent
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 2,
            "jar fetches should overlap on a worker pool"
        );
    }

    #[test]
    fn classpath_serializes_to_a_bucketed_json_object() {
        let classpath = Classpath {
            compile: vec![PathBuf::from("/c/a.jar"), PathBuf::from("/c/b.jar")],
            runtime: vec![PathBuf::from("/c/a.jar")],
            processor: vec![],
            api: vec![PathBuf::from("/c/a.jar")],
            compose_runtimes: vec![PathBuf::from("/c/compose-ui.jar")],
            test_compile: vec![PathBuf::from("/c/t.jar")],
            test_runtime: vec![PathBuf::from("/c/t.jar")],
            android_test_compile: vec![],
            android_test_runtime: vec![],
        };
        let json = classpath.to_json();
        assert_eq!(
            json,
            serde_json::json!({
                "compile": ["/c/a.jar", "/c/b.jar"],
                "runtime": ["/c/a.jar"],
                "processor": [],
                "api": ["/c/a.jar"],
                "composeRuntimes": ["/c/compose-ui.jar"],
                "testCompile": ["/c/t.jar"],
                "testRuntime": ["/c/t.jar"],
                "androidTestCompile": [],
                "androidTestRuntime": [],
            })
        );
    }

    #[test]
    fn empty_classpath_serializes_to_all_empty_buckets() {
        let json = Classpath::default().to_json();
        for bucket in [
            "compile",
            "runtime",
            "processor",
            "api",
            "composeRuntimes",
            "testCompile",
            "testRuntime",
            "androidTestCompile",
            "androidTestRuntime",
        ] {
            assert_eq!(json[bucket], serde_json::json!([]), "bucket {bucket}");
        }
    }

    #[test]
    fn missing_artifact_errors() {
        let repo = LocalRepo::new();
        let error = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::Implementation,
                "com.example:absent:1.0",
            )])
            .expect_err("not found");
        assert!(matches!(error, ResolveError::NotFound { .. }), "{error}");
    }

    #[test]
    fn bom_provides_version_for_versionless_dep() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "bom",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>lib</artifactId><version>3.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "lib",
            "3.0",
            &repo_pom("com.example", "lib", "3.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom:1.0"),
                declared(MavenScope::Implementation, "com.example:lib"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["lib-3.0.jar"]
        );
    }

    #[test]
    fn bom_versionless_dep_without_bom_is_skipped() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "lib",
            "1.0",
            &repo_pom("com.example", "lib", "1.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:lib")])
            .expect("resolves");
        assert!(resolution.classpath.compile.is_empty());
        assert!(
            resolution
                .notes
                .iter()
                .any(|n| n.contains("no version and no BOM"))
        );
        assert!(
            resolution.root_paths.is_empty(),
            "a declared version-less dep that stays unresolved contributes no root_paths entry"
        );
    }

    #[test]
    fn first_bom_wins_on_overlapping_managed_versions() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "bom-a",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom-a</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "bom-b",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom-b</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>lib</artifactId><version>2.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "lib",
            "1.0",
            &repo_pom("com.example", "lib", "1.0", &[]),
        );
        write_artifact(
            &repo.root,
            "com.example",
            "lib",
            "2.0",
            &repo_pom("com.example", "lib", "2.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom-a:1.0"),
                declared(MavenScope::Implementation, "com.example:bom-b:1.0"),
                declared(MavenScope::Implementation, "com.example:lib"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["lib-1.0.jar"]
        );
    }

    #[test]
    fn root_paths_lists_only_declared_roots_not_transitives() {
        let repo = LocalRepo::new();
        repo.add(
            "com.example",
            "lib",
            "1.0",
            &[("com.example:child", "2.0", "")],
        );
        repo.add("com.example", "child", "2.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[declared(MavenScope::Implementation, "com.example:lib:1.0")])
            .expect("resolves");
        assert_eq!(
            resolution.root_paths.keys().collect::<Vec<_>>(),
            vec![&("com.example".to_owned(), "lib".to_owned())],
            "only the declared root lib is keyed in root_paths, not its transitive child"
        );
        assert_eq!(
            resolution.root_paths[&("com.example".to_owned(), "lib".to_owned())]
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Some("lib-1.0.jar".to_owned()),
            "root_paths maps the lib coordinate to its own jar, not the child's"
        );
    }

    #[test]
    fn root_paths_omits_pom_packaging_declared_roots() {
        let repo = LocalRepo::new();
        // A BOM root is packaging `pom`, so it carries no jar.
        write_artifact(
            &repo.root,
            "com.example",
            "bom",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        repo.add("com.example", "lib", "1.0", &[]);
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom:1.0"),
                declared(MavenScope::Implementation, "com.example:lib"),
            ])
            .expect("resolves");
        assert_eq!(
            resolution.root_paths,
            BTreeMap::from([(
                ("com.example".to_owned(), "lib".to_owned()),
                resolution.classpath.compile[0].clone(),
            )]),
            "the BOM (pom packaging) and the version-managed lib resolve to one entry: lib's jar"
        );
    }

    #[test]
    fn bom_transitive_child_with_property_version_resolves_from_managed() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "bom",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>transitive</artifactId><version>5.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "parent",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>parent</artifactId><version>1.0</version>
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId><artifactId>transitive</artifactId><version>${managed.version}</version>
                </dependency>
              </dependencies>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "transitive",
            "5.0",
            &repo_pom("com.example", "transitive", "5.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom:1.0"),
                declared(MavenScope::Implementation, "com.example:parent:1.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["parent-1.0.jar", "transitive-5.0.jar"]
        );
        assert!(
            !resolution
                .notes
                .iter()
                .any(|n| n.contains("property version"))
        );
    }

    #[test]
    fn bom_child_dep_with_no_version_resolves_from_managed() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "bom",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>bom</artifactId><version>1.0</version>
              <packaging>pom</packaging>
              <dependencyManagement>
                <dependencies>
                  <dependency>
                    <groupId>com.example</groupId><artifactId>runtime-lib</artifactId><version>4.0</version>
                  </dependency>
                </dependencies>
              </dependencyManagement>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "lib-parent",
            "1.0",
            r#"<?xml version="1.0"?><project>
              <groupId>com.example</groupId><artifactId>lib-parent</artifactId><version>1.0</version>
              <dependencies>
                <dependency>
                  <groupId>com.example</groupId><artifactId>runtime-lib</artifactId>
                </dependency>
              </dependencies>
            </project>"#,
        );
        write_artifact(
            &repo.root,
            "com.example",
            "runtime-lib",
            "4.0",
            &repo_pom("com.example", "runtime-lib", "4.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[
                declared(MavenScope::Implementation, "com.example:bom:1.0"),
                declared(MavenScope::Implementation, "com.example:lib-parent:1.0"),
            ])
            .expect("resolves");
        assert_eq!(
            jar_names(&resolution.classpath.compile),
            vec!["lib-parent-1.0.jar", "runtime-lib-4.0.jar"]
        );
    }

    #[test]
    fn compile_only_version_managed_dep_without_bom_is_skipped() {
        let repo = LocalRepo::new();
        write_artifact(
            &repo.root,
            "com.example",
            "compile-only",
            "1.0",
            &repo_pom("com.example", "compile-only", "1.0", &[]),
        );
        let resolution = repo
            .resolver()
            .resolve(&[declared(
                MavenScope::CompileOnly,
                "com.example:compile-only",
            )])
            .expect("resolves");
        assert!(
            resolution.classpath.compile.is_empty(),
            "compileOnly dep with no BOM version must be skipped, not panic"
        );
        assert!(resolution.notes.iter().any(|n| n.contains("skipped")));
    }
}
