//! The core build driver: evaluate a project and execute the task graphs
//! its plugins register (ARCHITECTURE.md §9).
//!
//! [`build_project`] is the boundary between the module model and the task
//! engine: it reads a project's `conventions.ulb`/`libs.ulb`/`build.ulb`,
//! evaluates the resolved module model, resolves every plugin declared in
//! `plugins {}` against the registry, calls each plugin's `configure` entry
//! with the JSON serialization of the module model, merges the resulting
//! task graphs, and executes them incrementally over the fingerprint store.
//!
//! The module model is handed to each plugin whole rather than sliced per
//! plugin, because a plugin owns only the keys it recognizes and must be
//! free to read the configuration it is declared for; the fixture plugin,
//! for instance, reads `source` and `output` from the model and ignores the
//! rest. Paths inside those keys are interpreted by the plugin, which is
//! the only party that knows which keys are paths.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use ulb_lang::eval::Value;
use ulb_lang::token::Number;

use crate::host::PluginHost;
use crate::maven::{self, MavenRepo};
use crate::project::{self, read_libs_plugins};
use crate::registry::{Registry, RegistrySource};
use crate::task::{
    AllowlistedTool, BuildResult, Executor, FingerprintContext, FingerprintStore, TaskGraph,
};

/// The registry index consulted when [`BuildOptions`] does not name one.
pub const DEFAULT_REGISTRY: &str =
    "https://raw.githubusercontent.com/Ulite-Team/ulb-plugins/main/registry/index.json";

/// Options for a [`build_project`] run.
pub struct BuildOptions {
    /// Registry source for resolving the project's plugins. `None` uses
    /// [`DEFAULT_REGISTRY`].
    pub registry: Option<RegistrySource>,
    /// Cache directory for resolved plugin artifacts. `None` uses the
    /// registry's default cache location.
    pub cache_dir: Option<PathBuf>,
    /// Repositories for resolving the project's `deps {}` block. `None`
    /// uses Google Maven then Maven Central (ARCHITECTURE.md §7.1).
    pub repos: Option<Vec<MavenRepo>>,
}

/// Evaluates `dir` as a ulb project and runs the task graphs its plugins
/// register during `configure`.
///
/// The module model (the evaluated `build.ulb` top level) is serialized to
/// JSON and passed to each plugin's `configure` entry, so a plugin sees the
/// resolved configuration block its manifest declares itself for
/// (ARCHITECTURE.md §9 step 7). The project's `deps {}` block is resolved
/// host-side first and the resulting classpath is folded into that JSON as
/// a `classpath` key, so a plugin can embed the jar paths into its task
/// actions without resolving them itself. Every plugin's graph is merged
/// into one build; tasks are keyed by their `module` (the plugin name) and
/// name, so plugins cannot collide unless they register identical
/// identities. The merged graph is validated (unknown dependency, cycle)
/// and executed incrementally: unchanged tasks are skipped and the
/// fingerprint store is persisted to `<dir>/.uliab/state.json`.
///
/// The fingerprint context folds every resolved plugin's `name@version`
/// and a content-addressed hash of the configuration JSON (which includes
/// the resolved classpath), so upgrading a plugin, editing the project
/// sources, or changing the resolved dependencies reruns affected tasks.
///
/// # Errors
///
/// Returns a description of the failure when a project file cannot be read
/// or fails to parse or evaluate, when `libs.ulb` declares no plugins, when
/// a plugin cannot be resolved or refuses the configuration, when the
/// registered graphs cannot be merged or scheduled, or when the fingerprint
/// store cannot be persisted. Individual task failures are not errors; they
/// are reported in [`BuildResult::failure`].
///
/// # Examples
///
/// A full project: `build.ulb` declares the `source`/`output` keys the
/// fixture plugin owns, `libs.ulb` declares the plugin, and a local registry
/// index routes its coordinate to a fixture built for `wasm32-wasip2`. The
/// first run executes both registered tasks; the second skips them:
///
/// ```rust
/// use std::fs;
/// use std::path::PathBuf;
/// use std::process::Command;
///
/// use uliab::driver::{build_project, BuildOptions};
/// use uliab::registry::RegistrySource;
///
/// let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
///     .parent().unwrap().parent().unwrap().to_path_buf();
/// let built = Command::new("cargo")
///     .args(["build", "-p", "ulb-plugin-fixture", "--target", "wasm32-wasip2"])
///     .current_dir(&workspace)
///     .status().expect("build the fixture plugin");
/// assert!(built.success());
/// let target = std::env::var_os("CARGO_TARGET_DIR")
///     .map(PathBuf::from)
///     .unwrap_or_else(|| workspace.join("target"));
/// let fixture = target.join("wasm32-wasip2/debug/ulb_plugin_fixture.wasm");
///
/// let project = std::env::temp_dir().join(format!(
///     "uliab-driver-doc-{}", std::process::id()
/// ));
/// let _ = fs::remove_dir_all(&project);
/// fs::create_dir_all(&project).unwrap();
/// fs::write(project.join("in.txt"), "hello").unwrap();
/// fs::write(
///     project.join("build.ulb"),
///     format!(
///         "source = {:?}\noutput = {:?}\n",
///         project.join("in.txt").display().to_string(),
///         project.join("out.txt").display().to_string(),
///     ),
/// ).unwrap();
/// fs::write(
///     project.join("libs.ulb"),
///     "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
/// ).unwrap();
/// let index = project.join("index.json");
/// let index_doc = serde_json::json!({
///     "schema_version": 1,
///     "plugins": {
///         "ulite/fixture": {
///             "versions": {
///                 "0.1.0": {
///                     "abi": { "min": "0.3", "max": "0.3" },
///                     "artifact_url": fixture.display().to_string(),
///                 }
///             }
///         }
///     }
/// });
/// fs::write(&index, index_doc.to_string()).unwrap();
///
/// let options = BuildOptions {
///     registry: Some(RegistrySource::File(index)),
///     cache_dir: Some(project.join(".cache")),
///     repos: None,
/// };
/// let first = build_project(&project, &options).expect("first build");
/// assert_eq!((first.ran, first.up_to_date), (2, 0));
/// assert_eq!(fs::read(project.join("out.txt")).unwrap(), b"hello");
///
/// let second = build_project(&project, &options).expect("second build");
/// assert_eq!((second.ran, second.up_to_date), (0, 2));
/// ```
pub fn build_project(dir: &Path, options: &BuildOptions) -> Result<BuildResult, String> {
    let conventions = read_source(dir, "conventions.ulb", false)?;
    let libs = read_source(dir, "libs.ulb", true)?;
    let build = read_source(dir, "build.ulb", true)?;

    let outcome = ulb_lang::eval::evaluate_project(&conventions, &libs, &build);
    if !outcome.diagnostics.is_empty() {
        let messages = outcome
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "evaluating {}: {messages}",
            dir.join("build.ulb").display()
        ));
    }

    let model_json = module_model_to_json(&outcome.model)?;
    let repos = options
        .repos
        .clone()
        .unwrap_or_else(|| vec![maven::MavenRepo::Google, maven::MavenRepo::Central]);
    let classpath = match &outcome.model {
        Value::Block(entries) if entries.contains_key("deps") => {
            let resolution = resolve_model_deps(&outcome.model, &repos, options.cache_dir.clone())?;
            for note in &resolution.notes {
                eprintln!("note: {note}");
            }
            resolution.classpath
        }
        Value::Block(_) => maven::Classpath::default(),
        other => {
            return Err(format!(
                "the module model of {} is not a block (found {})",
                dir.display(),
                value_kind(other)
            ));
        }
    };

    // The classpath is resolved host-side and handed to every plugin as a
    // `classpath` key of its configuration, so a compiler or runner plugin
    // can embed the jar paths into its task actions without resolving them
    // itself. The hash that fingerprints the build covers the classpath
    // too, so a jar that changes without its version changing (a SNAPSHOT,
    // say) still reruns affected tasks.
    let mut plugin_config = model_json;
    let plugin_config_object = plugin_config
        .as_object_mut()
        .expect("the module model serialized to an object");
    // The classpath is resolved host-side and handed to every plugin as a
    // `classpath` key of its configuration, so a compiler or runner plugin
    // can embed the jar paths into its task actions without resolving them
    // itself. The hash that fingerprints the build covers the classpath
    // too, so a jar that changes without its version changing (a SNAPSHOT,
    // say) still reruns affected tasks.
    plugin_config_object.insert("classpath".to_owned(), classpath.to_json());
    // The project directory is handed over the same channel, so a plugin
    // can resolve its block's relative paths against it regardless of the
    // directory the build tool was invoked from.
    plugin_config_object.insert(
        "projectDir".to_owned(),
        serde_json::json!(dir.display().to_string()),
    );
    let config_text = plugin_config.to_string();
    let config_hash = hex(&Sha256::digest(config_text.as_bytes()));

    let libs = read_libs_plugins(dir)?;
    if libs.plugins.is_empty() {
        return Err(format!("{} declares no plugins", libs.libs_path.display()));
    }

    let source = options
        .registry
        .clone()
        .unwrap_or(RegistrySource::Url(DEFAULT_REGISTRY.to_owned()));
    let registry = Registry::new(source, options.cache_dir.clone());
    let host = PluginHost::new().map_err(|error| error.to_string())?;

    let mut graph = TaskGraph::new();
    let mut plugin_versions = Vec::new();
    for spec in &libs.plugins {
        let label = project::spec_label(spec);
        let resolved = registry
            .resolve(spec)
            .map_err(|error| format!("{label}: {error}"))?;
        if let Some(warning) = &resolved.warning {
            eprintln!("warning: {warning}");
        }
        let plugin_graph = host
            .configure(&resolved.path, &resolved.name, &config_text)
            .map_err(|error| format!("configuring {label}: {error}"))?;
        for task in plugin_graph.tasks() {
            graph
                .register(task.clone())
                .map_err(|error| format!("merging tasks from {label}: {error}"))?;
        }
        plugin_versions.push(format!("{}@{}", resolved.name, resolved.version));
    }

    let ctx = FingerprintContext {
        plugin_version: plugin_versions.join(","),
        config_hash,
    };
    let store_path = dir.join(".uliab").join("state.json");
    let mut store = FingerprintStore::load(&store_path)?;
    let executor = Executor::new([
        AllowlistedTool::Copy,
        AllowlistedTool::Cat,
        AllowlistedTool::Mkdir,
        AllowlistedTool::Echo,
        AllowlistedTool::Javac,
        AllowlistedTool::Kotlinc,
        AllowlistedTool::Jar,
        AllowlistedTool::Java,
    ]);
    let result = executor
        .execute(&graph, &ctx, &mut store)
        .map_err(|error| format!("scheduling the build: {error}"))?;
    store.save()?;
    Ok(result)
}

/// Reads one project source file. `conventions.ulb` is optional; the other
/// two are required for a build.
fn read_source(dir: &Path, name: &str, required: bool) -> Result<String, String> {
    let path = dir.join(name);
    match std::fs::read_to_string(&path) {
        Ok(source) => Ok(source),
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
            Ok(String::new())
        }
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// Serializes a resolved module model value to JSON.
///
/// Scalars map to their natural JSON types (`Version`/`Coordinate`/
/// `Properties` flatten to strings), blocks to objects, and lists to
/// arrays. Repeated scalar pair keys accumulate into a list upstream (see
/// the merge rule in `ulb-lang`'s evaluator), so a key that appears twice
/// in a block becomes an array rather than silently losing an entry.
fn module_model_to_json(model: &Value) -> Result<serde_json::Value, String> {
    match model {
        Value::Str(value) => Ok(serde_json::Value::String(value.clone())),
        Value::Number(number) => match number {
            Number::Int(value) => Ok(serde_json::json!(value)),
            Number::Float(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| format!("cannot serialize number {value}")),
        },
        Value::Bool(value) => Ok(serde_json::json!(value)),
        Value::List(items) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(module_model_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Version(value) => Ok(serde_json::Value::String(value.to_string())),
        Value::Coordinate(value) => Ok(serde_json::Value::String(value.clone())),
        Value::Properties(values) => Ok(serde_json::json!(values)),
        Value::Block(entries) => {
            let mut object = serde_json::Map::new();
            for (key, value) in entries {
                object.insert(key.clone(), module_model_to_json(value)?);
            }
            Ok(serde_json::Value::Object(object))
        }
        Value::Invalid(message) => Err(format!("cannot serialize an unresolved value: {message}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Resolves the `deps {}` block of the project at `dir` into a classpath,
/// consulting `repos` in order and caching artifacts under `cache_dir` (or
/// the default cache location when `None`).
///
/// The project's `deps {}` block is read from the evaluated module model
/// and resolved with [`maven::Resolver`] (ARCHITECTURE.md §6, §7). The
/// repositories are an explicit argument so callers that resolve against
/// local repositories stay offline; the CLI passes the default set.
///
/// # Errors
///
/// Returns a description when a project file cannot be read or fails to
/// parse or evaluate, when the model declares no `deps {}` block or the
/// block is malformed, or when resolution fails (see
/// [`maven::ResolveError`]).
///
/// # Examples
///
/// A local repository carries `example:one:1.0`, which depends on
/// `example:two:1.0`. Resolving the project's `deps {}` block materializes
/// both jars on the compile and runtime classpaths:
///
/// ```rust
/// use std::fs;
/// use uliab::driver::resolve_project_deps;
/// use uliab::maven::MavenRepo;
///
/// let dir = std::env::temp_dir().join(format!(
///     "uliab-deps-doc-{}", std::process::id()
/// ));
/// let _ = fs::remove_dir_all(&dir);
/// let repo = dir.join("repo");
/// fs::create_dir_all(repo.join("com/example/one/1.0")).unwrap();
/// fs::create_dir_all(repo.join("com/example/two/1.0")).unwrap();
/// fs::write(repo.join("com/example/one/1.0/one-1.0.pom"), r#"<?xml version="1.0"?>
/// <project><modelVersion>4.0.0</modelVersion>
/// <groupId>com.example</groupId><artifactId>one</artifactId><version>1.0</version>
/// <dependencies><dependency>
///   <groupId>com.example</groupId><artifactId>two</artifactId><version>1.0</version>
/// </dependency></dependencies></project>"#).unwrap();
/// fs::write(repo.join("com/example/two/1.0/two-1.0.pom"), r#"<?xml version="1.0"?>
/// <project><modelVersion>4.0.0</modelVersion>
/// <groupId>com.example</groupId><artifactId>two</artifactId><version>1.0</version>
/// </project>"#).unwrap();
/// fs::write(repo.join("com/example/one/1.0/one-1.0.jar"), b"one").unwrap();
/// fs::write(repo.join("com/example/two/1.0/two-1.0.jar"), b"two").unwrap();
/// fs::write(dir.join("build.ulb"), "deps {\n  implementation \"com.example:one:1.0\"\n}\n").unwrap();
/// fs::write(dir.join("libs.ulb"), "").unwrap();
///
/// let repos = vec![MavenRepo::Custom(repo.display().to_string())];
/// let resolution = resolve_project_deps(&dir, &repos, Some(dir.join("cache"))).expect("resolves");
/// assert_eq!(resolution.classpath.compile.len(), 2);
/// assert_eq!(resolution.classpath.runtime.len(), 2);
/// assert!(resolution.classpath.compile[0].ends_with("one-1.0.jar"));
/// ```
pub fn resolve_project_deps(
    dir: &Path,
    repos: &[MavenRepo],
    cache_dir: Option<PathBuf>,
) -> Result<maven::Resolution, String> {
    let conventions = read_source(dir, "conventions.ulb", false)?;
    let libs = read_source(dir, "libs.ulb", true)?;
    let build = read_source(dir, "build.ulb", true)?;

    let outcome = ulb_lang::eval::evaluate_project(&conventions, &libs, &build);
    if !outcome.diagnostics.is_empty() {
        let messages = outcome
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "evaluating {}: {messages}",
            dir.join("build.ulb").display()
        ));
    }

    resolve_model_deps(&outcome.model, repos, cache_dir)
}

/// Resolves the `deps {}` block of an evaluated module model, erroring when
/// the model declares none.
fn resolve_model_deps(
    model: &Value,
    repos: &[MavenRepo],
    cache_dir: Option<PathBuf>,
) -> Result<maven::Resolution, String> {
    let deps_block = match model {
        Value::Block(entries) => entries
            .get("deps")
            .ok_or_else(|| "the model does not declare a deps {} block".to_owned())?,
        other => {
            return Err(format!(
                "the module model is not a block (found {})",
                value_kind(other)
            ));
        }
    };
    let declared = maven::parse_deps_block(deps_block)?;
    let resolver = maven::Resolver::new(repos.to_vec(), cache_dir);
    resolver
        .resolve(&declared)
        .map_err(|error| error.to_string())
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Str(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::List(_) => "a list",
        Value::Version(_) => "a version",
        Value::Properties(_) => "properties",
        Value::Coordinate(_) => "a coordinate",
        Value::Block(_) => "a block",
        Value::Invalid(_) => "an unresolved value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_serializes_to_an_object_with_scalar_entries() {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("output".to_owned(), Value::Str("out.txt".to_owned()));
        entries.insert("source".to_owned(), Value::Str("in.txt".to_owned()));
        entries.insert("versionCode".to_owned(), Value::Number(Number::Int(7)));
        let json = module_model_to_json(&Value::Block(entries)).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "output": "out.txt",
                "source": "in.txt",
                "versionCode": 7,
            })
        );
    }

    #[test]
    fn block_serialization_is_deterministic_regardless_of_key_order() {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("b".to_owned(), Value::Str("x".to_owned()));
        entries.insert("a".to_owned(), Value::Str("y".to_owned()));
        let first = module_model_to_json(&Value::Block(entries)).expect("serializes");
        let mut reordered = std::collections::BTreeMap::new();
        reordered.insert("a".to_owned(), Value::Str("y".to_owned()));
        reordered.insert("b".to_owned(), Value::Str("x".to_owned()));
        let second = module_model_to_json(&Value::Block(reordered)).expect("serializes");
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn an_invalid_value_cannot_be_serialized() {
        let error = module_model_to_json(&Value::Invalid("unknown reference".to_owned()))
            .expect_err("invalid");
        assert!(error.contains("unresolved value"));
    }
}
