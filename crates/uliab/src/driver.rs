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
    split_cross_plugin_ref,
};

use std::collections::{HashMap, HashSet};

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
    /// An explicit Android SDK root, injected into each plugin's
    /// configuration as `androidSdkDir`. `None` probes the environment
    /// (`ANDROID_HOME`, then `ANDROID_SDK_ROOT`, then `~/Android/Sdk`);
    /// when none of those resolves to an existing directory the key is
    /// omitted and a plugin that needs it must be given the root another
    /// way (e.g. its own module block).
    pub android_sdk: Option<PathBuf>,
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
///                     "abi": { "min": "0.4", "max": "0.7" },
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
///     android_sdk: None,
/// };
/// let first = build_project(&project, &options).expect("first build");
/// assert_eq!((first.ran, first.up_to_date), (2, 0));
/// assert_eq!(fs::read(project.join("out.txt")).unwrap(), b"hello");
///
/// let second = build_project(&project, &options).expect("second build");
/// assert_eq!((second.ran, second.up_to_date), (0, 2));
/// ```
pub fn build_project(dir: &Path, options: &BuildOptions) -> Result<BuildResult, String> {
    // An absolute project directory keeps the plugin-config injection and a
    // module's derived build paths (which a plugin resolves against
    // `projectDir`) independent of the directory the tool was invoked from.
    let dir = canonical_project_dir(dir)?;

    // When a `settings.ulb` exists the project is multi-module: each
    // declared module has its own `build.ulb`, and the project-wide
    // `conventions.ulb`/`libs.ulb` are shared across them. `read_settings`
    // returns `Ok(None)` when the file does not exist, so a single read
    // decides the build path without a redundant existence check.
    if let Some(settings) = project::read_settings(&dir)? {
        return build_project_multi(&dir, options, settings);
    }

    build_project_single(&dir, options)
}

/// Single-module build path: evaluates one `build.ulb` at the project root.
fn build_project_single(dir: &Path, options: &BuildOptions) -> Result<BuildResult, String> {
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
    reject_project_refs(&outcome.model)?;
    let repos = options
        .repos
        .clone()
        .unwrap_or_else(|| vec![maven::MavenRepo::Google, maven::MavenRepo::Central]);
    let mut classpath = match &outcome.model {
        Value::Block(entries)
            if entries.contains_key("deps") || compose_deps(&outcome.model)?.is_some() =>
        {
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
    let Some(plugin_config_object) = plugin_config.as_object_mut() else {
        return Err(format!(
            "the module model of {} is not a block (found {})",
            dir.display(),
            value_kind(&outcome.model)
        ));
    };

    let source_sets =
        resolve_source_set_classpaths(&outcome.model, &repos, options.cache_dir.clone())?;
    if !source_sets.is_empty() {
        let mut source_set_map = serde_json::Map::new();
        for (path, source_set_classpath) in &source_sets {
            for jar in &source_set_classpath.api {
                if !classpath.api.contains(jar) {
                    classpath.api.push(jar.clone());
                }
            }
            source_set_map.insert(path.clone(), source_set_classpath.to_json());
        }
        plugin_config_object.insert(
            "classpathSourceSets".to_owned(),
            serde_json::Value::Object(source_set_map),
        );
    }

    plugin_config_object.insert("classpath".to_owned(), classpath.to_json());
    // The project directory is handed over the same channel, so a plugin
    // can resolve its block's relative paths against it regardless of the
    // directory the build tool was invoked from.
    plugin_config_object.insert(
        "projectDir".to_owned(),
        serde_json::json!(dir.display().to_string()),
    );
    // The Android SDK root, when one can be found, is handed over two
    // channels. The `androidSdkDir` configuration key tells a plugin where
    // the SDK is; the host also preopens the same directory read-only into
    // the plugin's WASI filesystem at its real path (see
    // `PluginHost::with_android_sdk`), so a plugin that discovers SDK
    // components — platform jars, build-tools binaries — can inspect it
    // itself. An explicit `--android-sdk` wins and must name an existing
    // directory — an override that resolves to nothing is an error rather
    // than a silent fallback to whatever the environment happens to have.
    // A module that declares its own `android.sdkDir` for the plugin gets
    // the same capability: that path is preopened read-only too, resolved
    // against the project directory when relative. Both are applied whether
    // or not the module declares an `android {}` block — the plugin decides
    // what a key means, and a project that never resolves an android plugin
    // simply ignores them.
    let sdk_root = checked_sdk_root(options.android_sdk.as_deref())?;
    if let Some(sdk_root) = &sdk_root {
        plugin_config_object.insert(
            "androidSdkDir".to_owned(),
            serde_json::json!(sdk_root.display().to_string()),
        );
    }
    let module_sdk_root = module_android_sdk_dir(&plugin_config, dir);
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
    let mut sdk_roots = Vec::new();
    if let Some(root) = &sdk_root {
        sdk_roots.push(root.clone());
    }
    if let Some(module_root) = module_sdk_root
        && !sdk_roots.contains(&module_root)
    {
        sdk_roots.push(module_root);
    }
    let host = sdk_roots
        .into_iter()
        .fold(host, |host, root| host.with_android_sdk(root));

    let mut graph = TaskGraph::new();
    let mut plugin_versions = Vec::new();
    let mut plugin_tasks: HashMap<String, HashSet<String>> = HashMap::new();
    let mut declared_deps: Vec<(String, Vec<String>, HashSet<String>)> = Vec::new();
    for spec in &libs.plugins {
        let label = project::spec_label(spec);
        let resolved = registry
            .resolve(spec)
            .map_err(|error| format!("{label}: {error}"))?;
        if let Some(warning) = &resolved.warning {
            eprintln!("warning: {warning}");
        }
        let result = host
            .configure(&resolved.path, &resolved.name, &config_text, dir)
            .map_err(|error| format!("configuring {label}: {error}"))?;
        let task_names: HashSet<String> = result.graph.tasks().map(|t| t.name.clone()).collect();
        plugin_tasks.insert(resolved.name.clone(), task_names);
        // Validate that every cross-plugin reference in this plugin's
        // tasks names a plugin listed in its declared `dependencies`.
        let deps_set: HashSet<&str> = result.dependencies.iter().map(|s| s.as_str()).collect();
        let mut used_deps: HashSet<String> = HashSet::new();
        for task in result.graph.tasks() {
            for dep in &task.depends_on {
                if let Some((provider, task_name)) = split_cross_plugin_ref(dep) {
                    used_deps.insert(provider.to_owned());
                    if !deps_set.contains(provider) {
                        return Err(format!(
                            "plugin '{}' references '{}:{}' in task '{}' but does not declare '{}' in its dependencies",
                            resolved.name, provider, task_name, task.name, provider
                        ));
                    }
                }
            }
        }
        declared_deps.push((resolved.name.clone(), result.dependencies, used_deps));
        for task in result.graph.tasks() {
            graph
                .register(task.clone())
                .map_err(|error| format!("merging tasks from {label}: {error}"))?;
        }
        plugin_versions.push(format!("{}@{}", resolved.name, resolved.version));
    }

    // Validate declared dependencies that are actually referenced in cross-
    // plugin task refs are present in the build. Declared but unreferenced
    // dependencies are allowed — a plugin may declare optional capabilities
    // that not every module configuration activates.
    for (plugin_name, deps, used_deps) in &declared_deps {
        for dep in deps {
            if used_deps.contains(dep.as_str()) && !plugin_tasks.contains_key(dep.as_str()) {
                return Err(format!(
                    "plugin '{plugin_name}' declares and references dependency '{dep}', which is not present in the build"
                ));
            }
        }
    }
    graph
        .resolve_cross_plugin_deps(&|_consumer_module, plugin, task| {
            // Single-module builds label every plugin's tasks with the
            // plugin name alone, so the provider of any reference is
            // `<plugin>::<task>`. Unknown providers surface from the wave
            // computation as unknown dependencies naming that key.
            Some(format!("{plugin}::{task}"))
        })
        .map_err(|error| format!("resolving cross-plugin dependencies: {error}"))?;

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
        AllowlistedTool::Aapt2,
        AllowlistedTool::Apksigner,
    ]);
    let result = executor
        .execute(&graph, &ctx, &mut store)
        .map_err(|error| format!("scheduling the build: {error}"))?;
    store.save()?;
    Ok(result)
}

/// Multi-module build path: evaluates each module's `build.ulb` declared in
/// `settings.ulb`, sharing the project-wide `conventions.ulb`/`libs.ulb`.
///
/// Each module is built independently — its `build.ulb` is evaluated against
/// the project-root conventions and libs, its `deps {}` is resolved, and
/// every plugin declared in the project's `libs.ulb` is configured with that
/// module's model. Task identities are prefixed with the module path
/// (`<module>/<plugin>::<task>`) so two modules using the same plugin
/// produce distinct tasks. All module graphs are merged into one build and
/// executed together over a shared fingerprint store at the project root.
///
/// # Errors
///
/// Returns an error when `settings.ulb` cannot be read or evaluated, when a
/// module directory does not exist, when a module's `build.ulb` is missing
/// or fails to evaluate, when a plugin refuses a module's configuration, or
/// when the merged task graph has cycles or unknown dependencies.
fn build_project_multi(
    dir: &Path,
    options: &BuildOptions,
    settings: project::ProjectSettings,
) -> Result<BuildResult, String> {
    let conventions = read_source(dir, "conventions.ulb", false)?;
    let libs_src = read_source(dir, "libs.ulb", true)?;

    let libs_project = read_libs_plugins(dir)?;
    if libs_project.plugins.is_empty() {
        return Err(format!(
            "{} declares no plugins",
            libs_project.libs_path.display()
        ));
    }

    let source = options
        .registry
        .clone()
        .unwrap_or(RegistrySource::Url(DEFAULT_REGISTRY.to_owned()));
    let registry = Registry::new(source, options.cache_dir.clone());
    let sdk_root = checked_sdk_root(options.android_sdk.as_deref())?;

    let mut repos = options
        .repos
        .clone()
        .unwrap_or_else(|| vec![maven::MavenRepo::Google, maven::MavenRepo::Central]);
    for url in settings.model.extra_repos.iter().rev() {
        repos.insert(0, maven::MavenRepo::Custom(url.clone()));
    }

    // ── Pass 1: evaluate every module and discover its output artifact. ──
    // This must happen before dependency resolution so that cross-module
    // `project(":shared")` refs can locate the target module's jar/apk.
    struct EvaluatedModule {
        rel: String,
        dir: PathBuf,
        model: Value,
        output: Option<PathBuf>,
    }
    let mut modules: Vec<EvaluatedModule> = Vec::with_capacity(settings.module_dirs.len());
    for (module_dir, module_rel) in settings
        .module_dirs
        .iter()
        .zip(settings.model.modules.iter())
    {
        if !module_dir.is_dir() {
            return Err(format!(
                "module directory '{}' does not exist (declared in settings.ulb as '{module_rel}')",
                module_dir.display()
            ));
        }

        let build = read_source(module_dir, "build.ulb", true)?;
        let outcome = ulb_lang::eval::evaluate_project(&conventions, &libs_src, &build);
        if !outcome.diagnostics.is_empty() {
            let messages = outcome
                .diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "evaluating {}/build.ulb: {messages}",
                module_dir.display()
            ));
        }

        let output = discover_module_output(&outcome.model, module_dir);
        modules.push(EvaluatedModule {
            rel: module_rel.clone(),
            dir: module_dir.clone(),
            model: outcome.model,
            output,
        });
    }

    // Build the module-rel → output map for project-dep resolution.
    let module_outputs: std::collections::HashMap<String, PathBuf> = modules
        .iter()
        .filter_map(|m| Some((m.rel.clone(), m.output.clone()?)))
        .collect();

    // ── Pass 2a: resolve every module's Maven dependencies — top-level and
    // per-source-set — and record each module's api classpath. Cross-module
    // `project(":…")` references are only resolved afterwards (Pass 2b), so
    // declaration order in settings.ulb never matters: a consumer listed
    // before its dependency still sees the dependency's api jars.
    struct PreparedModule {
        rel: String,
        dir: PathBuf,
        model_json: serde_json::Value,
        classpath: maven::Classpath,
        source_sets: Vec<(String, maven::Classpath)>,
        top_level_refs: ProjectRefs,
        source_set_refs: Vec<(String, ProjectRefs)>,
    }
    let mut module_api_classpaths: std::collections::HashMap<String, Vec<PathBuf>> =
        std::collections::HashMap::new();
    let mut prepared: Vec<PreparedModule> = Vec::with_capacity(modules.len());
    for m in &modules {
        let model_json = module_model_to_json(&m.model)?;

        // Resolve Maven deps (project refs are skipped by parse_deps_block).
        let mut classpath = match &m.model {
            Value::Block(entries)
                if entries.contains_key("deps") || compose_deps(&m.model)?.is_some() =>
            {
                let resolution = resolve_model_deps(&m.model, &repos, options.cache_dir.clone())?;
                for note in &resolution.notes {
                    eprintln!("note: [{}] {}", m.rel, note);
                }
                resolution.classpath
            }
            Value::Block(_) => maven::Classpath::default(),
            other => {
                return Err(format!(
                    "the module model of {}/build.ulb is not a block (found {})",
                    m.dir.display(),
                    value_kind(other)
                ));
            }
        };

        let source_sets =
            resolve_source_set_classpaths(&m.model, &repos, options.cache_dir.clone())?;
        for (_, source_set_classpath) in &source_sets {
            for jar in &source_set_classpath.api {
                if !classpath.api.contains(jar) {
                    classpath.api.push(jar.clone());
                }
            }
        }
        module_api_classpaths.insert(m.rel.clone(), classpath.api.clone());

        // Extracting the references is order-independent; only resolving
        // them waits for the complete api-classpath map.
        let top_level_refs = match &m.model {
            Value::Block(entries) => entries
                .get("deps")
                .map(maven::extract_project_deps)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let source_set_refs = collect_source_set_project_refs(&m.model)?;

        prepared.push(PreparedModule {
            rel: m.rel.clone(),
            dir: m.dir.clone(),
            model_json,
            classpath,
            source_sets,
            top_level_refs,
            source_set_refs,
        });
    }

    // ── Pass 2b: resolve cross-module project references against the
    // complete api-classpath map, then configure every plugin.
    let mut graph = TaskGraph::new();
    let mut plugin_versions = Vec::new();
    let mut config_hashes = Vec::new();
    let mut all_plugin_tasks: HashMap<String, HashSet<String>> = HashMap::new();
    let mut all_plugin_names: HashSet<String> = HashSet::new();
    let mut all_declared_deps: Vec<(String, Vec<String>, HashSet<String>)> = Vec::new();

    for PreparedModule {
        rel,
        dir,
        model_json,
        mut classpath,
        mut source_sets,
        top_level_refs,
        source_set_refs,
    } in prepared
    {
        if !top_level_refs.is_empty() {
            let project_cp = resolve_project_classpath(
                &top_level_refs,
                &module_outputs,
                &module_api_classpaths,
                &settings.model.modules,
            )?;
            classpath.compile.extend(project_cp.compile);
            classpath.runtime.extend(project_cp.runtime);
            classpath.test_compile.extend(project_cp.test_compile);
            classpath.test_runtime.extend(project_cp.test_runtime);
        }
        for (path, refs) in &source_set_refs {
            let fragment = resolve_project_classpath(
                refs,
                &module_outputs,
                &module_api_classpaths,
                &settings.model.modules,
            )
            .map_err(|error| format!("{path}: {error}"))?;
            if let Some((_, source_set_classpath)) =
                source_sets.iter_mut().find(|(sp, _)| sp == path)
            {
                source_set_classpath.compile.extend(fragment.compile);
                source_set_classpath.runtime.extend(fragment.runtime);
                source_set_classpath
                    .test_compile
                    .extend(fragment.test_compile);
                source_set_classpath
                    .test_runtime
                    .extend(fragment.test_runtime);
            }
        }

        let mut plugin_config = model_json;
        let Some(plugin_config_object) = plugin_config.as_object_mut() else {
            return Err(format!(
                "the module model of {}/build.ulb is not a block",
                dir.display()
            ));
        };

        if !source_sets.is_empty() {
            let mut source_set_map = serde_json::Map::new();
            for (path, source_set_classpath) in &source_sets {
                source_set_map.insert(path.clone(), source_set_classpath.to_json());
            }
            plugin_config_object.insert(
                "classpathSourceSets".to_owned(),
                serde_json::Value::Object(source_set_map),
            );
        }

        plugin_config_object.insert("classpath".to_owned(), classpath.to_json());

        plugin_config_object.insert(
            "projectDir".to_owned(),
            serde_json::json!(dir.display().to_string()),
        );
        plugin_config_object.insert("modulePath".to_owned(), serde_json::json!(rel));

        if let Some(sdk_root) = &sdk_root {
            plugin_config_object.insert(
                "androidSdkDir".to_owned(),
                serde_json::json!(sdk_root.display().to_string()),
            );
        }
        let module_sdk_root = module_android_sdk_dir(&plugin_config, &dir);
        let mut sdk_roots_extra = Vec::new();
        if let Some(root) = &sdk_root {
            sdk_roots_extra.push(root.clone());
        }
        if let Some(module_root) = module_sdk_root
            && !sdk_roots_extra.contains(&module_root)
        {
            sdk_roots_extra.push(module_root);
        }
        let module_host = {
            let base = PluginHost::new().map_err(|error| error.to_string())?;
            sdk_roots_extra
                .into_iter()
                .fold(base, |h, root| h.with_android_sdk(root))
        };

        let config_text = plugin_config.to_string();
        config_hashes.push(hex(&Sha256::digest(config_text.as_bytes())));

        for spec in &libs_project.plugins {
            let label = format!("{}/{}", rel, project::spec_label(spec));
            let resolved = registry
                .resolve(spec)
                .map_err(|error| format!("{label}: {error}"))?;
            if let Some(warning) = &resolved.warning {
                eprintln!("warning: {label}: {warning}");
            }

            let module_prefixed_name = format!("{}/{}", rel, resolved.name);

            let result = module_host
                .configure(&resolved.path, &module_prefixed_name, &config_text, &dir)
                .map_err(|error| format!("configuring {label}: {error}"))?;
            let task_names: HashSet<String> =
                result.graph.tasks().map(|t| t.name.clone()).collect();
            // Union task names across modules so the cross-plugin resolution
            // index covers every registered task.
            all_plugin_tasks
                .entry(resolved.name.clone())
                .or_default()
                .extend(task_names);
            all_plugin_names.insert(resolved.name.clone());
            // Validate that every cross-plugin reference in this plugin's
            // tasks names a plugin listed in its declared `dependencies`.
            let deps_set: HashSet<&str> = result.dependencies.iter().map(|s| s.as_str()).collect();
            let mut used_deps: HashSet<String> = HashSet::new();
            for task in result.graph.tasks() {
                for dep in &task.depends_on {
                    if let Some((provider, task_name)) = split_cross_plugin_ref(dep) {
                        used_deps.insert(provider.to_owned());
                        if !deps_set.contains(provider) {
                            return Err(format!(
                                "plugin '{}' references '{}:{}' in task '{}' but does not declare '{}' in its dependencies",
                                resolved.name, provider, task_name, task.name, provider
                            ));
                        }
                    }
                }
            }
            all_declared_deps.push((resolved.name.clone(), result.dependencies, used_deps));
            for task in result.graph.tasks() {
                let t: crate::task::Task = task.clone();
                graph
                    .register(t)
                    .map_err(|error| format!("merging tasks from {label}: {error}"))?;
            }
            plugin_versions.push(format!("{}/{}@{}", rel, resolved.name, resolved.version));
        }
    }

    // Validate declared dependencies that are actually referenced in cross-
    // plugin task refs are present in the build.
    for (plugin_name, deps, used_deps) in &all_declared_deps {
        for dep in deps {
            if used_deps.contains(dep.as_str()) && !all_plugin_tasks.contains_key(dep.as_str()) {
                return Err(format!(
                    "plugin '{plugin_name}' declares and references dependency '{dep}', which is not present in the build"
                ));
            }
        }
    }
    graph
        .resolve_cross_plugin_deps(&|consumer_module, plugin, task| {
            let label = provider_module_label(&all_plugin_names, consumer_module, plugin)?;
            Some(format!("{label}::{task}"))
        })
        .map_err(|error| format!("resolving cross-plugin dependencies: {error}"))?;

    let ctx = FingerprintContext {
        plugin_version: plugin_versions.join(","),
        config_hash: config_hashes.join(","),
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
        AllowlistedTool::Aapt2,
        AllowlistedTool::Apksigner,
    ]);
    let result = executor
        .execute(&graph, &ctx, &mut store)
        .map_err(|error| format!("scheduling the build: {error}"))?;
    store.save()?;
    Ok(result)
}

/// Resolves the Android SDK root a build should use, applying the
/// explicit-override rule [`build_project`] depends on: an override that
/// does not name an existing directory is an error — the build must not
/// silently probe the environment and compile against a different SDK than
/// the user asked for. With no override, `ANDROID_HOME`, then
/// `ANDROID_SDK_ROOT`, then the conventional `~/Android/Sdk` are probed
/// silently; the first existing directory wins, and `None` when none
/// exists (the plugins then report their own missing-SDK errors). The
/// resolved root is injected into every plugin's configuration as
/// `androidSdkDir` (see [`build_project`]).
///
/// # Errors
///
/// Returns a message naming the override when an explicit path does not
/// exist.
pub fn checked_sdk_root(override_path: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match override_path {
        Some(path) if !path.is_dir() => Err(format!(
            "the Android SDK root '{}' does not exist (--android-sdk must name an existing directory)",
            path.display()
        )),
        Some(path) => Ok(Some(path.to_path_buf())),
        None => Ok(android_sdk_root(None)),
    }
}

/// Resolves the Android SDK root by probing the environment conventions:
/// an explicit `--android-sdk` path wins, then `ANDROID_HOME`, then
/// `ANDROID_SDK_ROOT`, then the conventional `~/Android/Sdk`. The first
/// candidate that is an existing directory wins; `None` when no candidate
/// exists. The environment variables are read at call time, which is why
/// the ordering itself is tested through [`sdk_candidates`] rather than by
/// mutating the process environment in a test. [`build_project`]
/// hard-fails on an explicit override that does not exist via
/// [`checked_sdk_root`]; the resolved root is injected into every plugin's
/// configuration as `androidSdkDir`.
pub fn android_sdk_root(override_path: Option<&Path>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    first_existing_sdk_dir(&sdk_candidates(override_path, home.as_deref()))
}

/// The module's own `android.sdkDir` — a block path the plugin is asked to
/// use instead of the host-resolved root — resolved against the project
/// directory when relative. `None` when the module declares none, or
/// declares something the host cannot turn into a path (the plugin reports
/// such a block as its own error). The host preopens this directory
/// read-only too (see [`PluginHost::with_android_sdk`]), so a per-module
/// SDK a plugin discovers is actually readable from the guest filesystem
/// rather than being visible only as a JSON string.
fn module_android_sdk_dir(plugin_config: &serde_json::Value, dir: &Path) -> Option<PathBuf> {
    let sdk_dir = plugin_config.get("android")?.get("sdkDir")?.as_str()?;
    let path = Path::new(sdk_dir);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(dir.join(path))
    }
}

/// The Android SDK roots a build probes, in priority order. A pure
/// function of the override and the home directory (the environment
/// variables are read separately) so the ordering is testable without
/// touching the process environment.
fn sdk_candidates(override_path: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = override_path {
        candidates.push(path.to_path_buf());
    }
    for variable in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(path) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(path));
        }
    }
    if let Some(home) = home {
        candidates.push(home.join("Android").join("Sdk"));
    }
    candidates
}

/// The first candidate that is an existing directory, or `None`.
fn first_existing_sdk_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|candidate| candidate.is_dir())
        .cloned()
}

/// Resolves the project directory to an absolute path, so the `projectDir`
/// a plugin receives and the build paths derived from it are correct no
/// matter which directory the tool was invoked from. A project path that
/// does not exist is a configure-time error, not a mid-build surprise.
fn canonical_project_dir(dir: &Path) -> Result<PathBuf, String> {
    dir.canonicalize().map_err(|error| {
        format!(
            "cannot resolve project directory '{}': {error}",
            dir.display()
        )
    })
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
        Value::ProjectRef(path) => {
            let mut map = serde_json::Map::new();
            map.insert(
                "project".to_owned(),
                serde_json::Value::String(path.clone()),
            );
            Ok(serde_json::Value::Object(map))
        }
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
    let outcome = evaluate_project_dir(dir)?;
    resolve_model_deps(&outcome.model, repos, cache_dir)
}

/// Resolves every source-set `deps {}` block declared in the project at
/// `dir` into its own classpath, consulting `repos` in order and caching
/// artifacts under `cache_dir` (or the default cache location when `None`).
///
/// A source set is any block below the module level that carries a `deps`
/// key — `commonMain`, `jvmMain`, `androidMain`, or deeper nesting such as
/// `kmp.commonMain` — and each resolves independently, so a module can keep
/// one set of dependencies visible to its shared sources and another to a
/// single target. The module's own top-level `deps {}` block is *not*
/// included; use [`resolve_project_deps`] for that. The result is ordered by
/// source-set path.
///
/// # Errors
///
/// Returns a description when a project file cannot be read or fails to
/// parse or evaluate, when a source-set deps block is malformed (including a
/// `deps` that is not a block), or when resolution fails (see
/// [`maven::ResolveError`]).
///
/// # Examples
///
/// A local repository carries `example:one:1.0`. The module declares it for
/// `commonMain` and for `androidMain`, and each source set resolves
/// independently:
///
/// ```rust
/// use std::fs;
/// use uliab::driver::resolve_project_source_sets;
/// use uliab::maven::MavenRepo;
///
/// let dir = std::env::temp_dir().join(format!(
///     "uliab-source-sets-doc-{}", std::process::id()
/// ));
/// let _ = fs::remove_dir_all(&dir);
/// let repo = dir.join("repo");
/// fs::create_dir_all(repo.join("com/example/one/1.0")).unwrap();
/// fs::write(repo.join("com/example/one/1.0/one-1.0.pom"), r#"<?xml version="1.0"?>
/// <project><modelVersion>4.0.0</modelVersion>
/// <groupId>com.example</groupId><artifactId>one</artifactId><version>1.0</version>
/// </project>"#).unwrap();
/// fs::write(repo.join("com/example/one/1.0/one-1.0.jar"), b"one").unwrap();
/// fs::write(dir.join("build.ulb"), r#"commonMain.deps {
///   implementation "com.example:one:1.0"
/// }
/// androidMain.deps {
///   implementation "com.example:one:1.0"
/// }"#).unwrap();
/// fs::write(dir.join("libs.ulb"), "").unwrap();
///
/// let repos = vec![MavenRepo::Custom(repo.display().to_string())];
/// let resolved =
///     resolve_project_source_sets(&dir, &repos, Some(dir.join("cache"))).expect("resolves");
/// let paths: Vec<&str> = resolved.iter().map(|(path, _)| path.as_str()).collect();
/// assert_eq!(paths, ["androidMain", "commonMain"]);
/// for (_, classpath) in &resolved {
///     assert_eq!(classpath.compile.len(), 1);
/// }
/// ```
pub fn resolve_project_source_sets(
    dir: &Path,
    repos: &[MavenRepo],
    cache_dir: Option<PathBuf>,
) -> Result<Vec<(String, maven::Classpath)>, String> {
    let outcome = evaluate_project_dir(dir)?;
    resolve_source_set_classpaths(&outcome.model, repos, cache_dir)
}

/// Evaluates the project sources at `dir` (`conventions.ulb`, `libs.ulb`,
/// `build.ulb`) into a module model, erroring on the first diagnostic.
fn evaluate_project_dir(dir: &Path) -> Result<ulb_lang::eval::EvalOutcome, String> {
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
    Ok(outcome)
}

/// Default Compose BOM version injected when `compose = true` but no
/// `composeVersion` is specified in the `android {}` block.
const DEFAULT_COMPOSE_BOM_VERSION: &str = "2026.08.00";

/// When `android.compose = true` in the module model, returns the
/// Compose BOM and standard runtime/UI deps that should be injected
/// into the resolution. The BOM is declared with an explicit version;
/// the standard artifacts are version-less (resolved from the BOM's
/// `dependencyManagement`).
///
/// The `composeVersion` key in the `android {}` block specifies the
/// Compose BOM version (e.g. `"2026.08.00"` or `ver("2026.08.00")`);
/// when omitted the default above is injected.
///
/// # Errors
///
/// Returns an error when `composeVersion` is present but not displayable
/// as a coordinate version segment.
fn compose_deps(model: &Value) -> Result<Option<Vec<maven::DeclaredDep>>, String> {
    let android = match model {
        Value::Block(entries) => entries.get("android"),
        _ => return Ok(None),
    };
    let Some(android) = android else {
        return Ok(None);
    };
    let compose = match android {
        Value::Block(entries) => entries.get("compose"),
        _ => return Ok(None),
    };
    let Some(compose) = compose else {
        return Ok(None);
    };
    if !matches!(compose, Value::Bool(true)) {
        return Ok(None);
    }
    let compose_version = match android {
        Value::Block(entries) => match entries.get("composeVersion") {
            Some(Value::Str(v)) => v.clone(),
            Some(v) => v.as_display_string().ok_or_else(|| {
                "android.composeVersion must be a string or version value".to_owned()
            })?,
            None => DEFAULT_COMPOSE_BOM_VERSION.to_owned(),
        },
        _ => DEFAULT_COMPOSE_BOM_VERSION.to_owned(),
    };
    let scope = maven::MavenScope::Implementation;
    let bom = maven::DeclaredDep {
        scope,
        dependency: maven::Dependency::parse(&format!(
            "androidx.compose:compose-bom:{compose_version}"
        ))
        .map_err(|error| format!("invalid android.composeVersion '{compose_version}': {error}"))?,
    };
    let managed = ["runtime", "ui", "material3"]
        .into_iter()
        .map(|artifact| maven::DeclaredDep {
            scope,
            dependency: maven::Dependency::parse(&format!(
                "androidx.compose.{artifact}:{artifact}"
            ))
            .expect("valid coordinate"),
        })
        .collect::<Vec<_>>();
    let mut deps = vec![bom];
    deps.extend(managed);
    Ok(Some(deps))
}

/// Resolves the `deps {}` block of an evaluated module model, erroring when
/// the model declares none.
fn resolve_model_deps(
    model: &Value,
    repos: &[MavenRepo],
    cache_dir: Option<PathBuf>,
) -> Result<maven::Resolution, String> {
    let deps_block = match model {
        Value::Block(entries) => entries.get("deps"),
        _ => None,
    };
    let mut declared = match deps_block {
        Some(block) => maven::parse_deps_block(block)?,
        None => Vec::new(),
    };
    if let Some(compose) = compose_deps(model)? {
        declared.extend(compose);
    }
    if declared.is_empty() {
        return Err("the model does not declare a deps {} block".to_owned());
    }
    let resolver = maven::Resolver::new(repos.to_vec(), cache_dir);
    resolver
        .resolve(&declared)
        .map_err(|error| error.to_string())
}

/// Resolves every source-set `deps {}` block in the module model into its
/// own classpath (see [`resolve_project_source_sets`]). When
/// `android.compose = true`, compose BOM and runtime/UI/Material 3 deps
/// are injected into every source-set classpath so the compose-managed
/// libraries are available to all targets without repeating the BOM
/// declaration in each source set.
fn resolve_source_set_classpaths(
    model: &Value,
    repos: &[MavenRepo],
    cache_dir: Option<PathBuf>,
) -> Result<Vec<(String, maven::Classpath)>, String> {
    let top = match model {
        Value::Block(entries) => entries,
        other => {
            return Err(format!(
                "the module model is not a block (found {})",
                value_kind(other)
            ));
        }
    };
    let compose = compose_deps(model)?;
    let mut blocks = Vec::new();
    for (key, value) in top {
        let mut path = vec![key.clone()];
        collect_source_set_deps(&mut blocks, &mut path, value)?;
    }
    let mut resolved = Vec::new();
    for (path, deps) in blocks {
        let mut declared = maven::parse_deps_block(deps)?;
        if let Some(ref compose) = compose {
            declared.extend(compose.iter().cloned());
        }
        let resolver = maven::Resolver::new(repos.to_vec(), cache_dir.clone());
        let classpath = resolver
            .resolve(&declared)
            .map_err(|error| format!("{path}: {error}"))?
            .classpath;
        resolved.push((path, classpath));
    }
    Ok(resolved)
}

/// A scope paired with the module path it references (`":shared"`), as
/// extracted from a `deps {}` block by [`maven::extract_project_deps`].
type ProjectRefs = Vec<(maven::MavenScope, String)>;

/// Collects the `project(":module")` references declared inside every
/// source-set `deps {}` block of the module model, keyed by the same dotted
/// paths [`resolve_source_set_classpaths`] uses. Extraction is separated
/// from resolution so a multi-module build can gather every module's
/// references first and resolve them only once each module's api classpath
/// is known — declaration order in settings.ulb then plays no role.
fn collect_source_set_project_refs(model: &Value) -> Result<Vec<(String, ProjectRefs)>, String> {
    let top = match model {
        Value::Block(entries) => entries,
        other => {
            return Err(format!(
                "the module model is not a block (found {})",
                value_kind(other)
            ));
        }
    };
    let mut blocks = Vec::new();
    for (key, value) in top {
        let mut path = vec![key.clone()];
        collect_source_set_deps(&mut blocks, &mut path, value)?;
    }
    let mut refs = Vec::new();
    for (path, deps) in blocks {
        let project_deps = maven::extract_project_deps(deps);
        if !project_deps.is_empty() {
            refs.push((path, project_deps));
        }
    }
    Ok(refs)
}

/// Errors when the module model declares any `project(":…")` dependency,
/// at the top level or inside a nested source set. Only a multi-module
/// build can satisfy such references; a single-module build has no other
/// modules to resolve them against.
fn reject_project_refs(model: &Value) -> Result<(), String> {
    let mut locations = Vec::new();
    if let Value::Block(entries) = model
        && let Some(deps) = entries.get("deps")
        && !maven::extract_project_deps(deps).is_empty()
    {
        locations.push("deps".to_owned());
    }
    for path in collect_source_set_project_refs(model)? {
        locations.push(format!("{}.deps", path.0));
    }
    if locations.is_empty() {
        return Ok(());
    }
    Err(format!(
        "project() dependencies are only valid in a multi-module build with a settings.ulb \
         declaring more than one module (found at {})",
        locations.join(", ")
    ))
}

/// Maps a cross-plugin reference onto the module label the provider's
/// tasks are registered under.
///
/// Multi-module builds label tasks `<module>/<plugin>`, so the consumer's
/// own label yields the project-module prefix by stripping the longest
/// known plugin name it ends with; single-module builds label tasks with
/// the bare plugin name, detected by an exact match. Returns `None` when
/// no layout fits, which surfaces as an unknown-dependency error naming
/// the reference.
fn provider_module_label(
    plugin_names: &HashSet<String>,
    consumer_module: &str,
    provider_plugin: &str,
) -> Option<String> {
    if plugin_names.contains(consumer_module) {
        return Some(provider_plugin.to_owned());
    }
    let mut best: Option<&str> = None;
    for name in plugin_names {
        let sep = format!("/{name}");
        if consumer_module.ends_with(&sep) && best.is_none_or(|longest| name.len() > longest.len())
        {
            best = Some(name);
        }
    }
    let matched = best?;
    let rel = &consumer_module[..consumer_module.len() - matched.len() - 1];
    Some(format!("{rel}/{provider_plugin}"))
}

/// Collects every `deps` block nested at or below `value`, recording each
/// under its key path joined with `.` (`commonMain`, `kmp.commonMain`). A
/// block's own `deps` key is recorded but never descended into, so nested
/// declarations cannot shadow each other, and non-block values are reported
/// with their path so the error points at the offending source set.
fn collect_source_set_deps<'a>(
    out: &mut Vec<(String, &'a Value)>,
    path: &mut Vec<String>,
    value: &'a Value,
) -> Result<(), String> {
    let Value::Block(entries) = value else {
        return Ok(());
    };
    if let Some(deps) = entries.get("deps") {
        match deps {
            Value::Block(_) => out.push((path.join("."), deps)),
            other => {
                return Err(format!(
                    "deps at '{}' must be a block (found {})",
                    path.join("."),
                    value_kind(other)
                ));
            }
        }
    }
    for (key, child) in entries {
        if key == "deps" || !matches!(child, Value::Block(_)) {
            continue;
        }
        path.push(key.clone());
        collect_source_set_deps(out, path, child)?;
        path.pop();
    }
    Ok(())
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
        Value::ProjectRef(_) => "a project reference",
    }
}

/// Resolves a relative path against a base directory.
fn resolve_path(base: &Path, relative: &str) -> PathBuf {
    let path = Path::new(relative);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Discovers a module's primary output artifact from its evaluated model.
///
/// Scans well-known output keys owned by plugin families (`jvm.jarFile`,
/// `android.apk`) and resolves them against the module directory. Returns
/// `None` when no recognized output key is present — the module either
/// produces no file artifact (a library consumed only via classpath) or
/// its plugin family defines no output key this scan recognizes.
fn discover_module_output(model: &Value, module_dir: &Path) -> Option<PathBuf> {
    let block = match model {
        Value::Block(entries) => entries,
        _ => return None,
    };
    // JVM plugin: `jvm { jarFile "..." }`.
    if let Some(Value::Block(jvm)) = block.get("jvm")
        && let Some(Value::Str(jar)) = jvm.get("jarFile")
    {
        return Some(resolve_path(module_dir, jar));
    }
    // Android plugin: `android { apk "..." }`.
    if let Some(Value::Block(android)) = block.get("android")
        && let Some(Value::Str(apk)) = android.get("apk")
    {
        return Some(resolve_path(module_dir, apk));
    }
    None
}

/// Builds a [`maven::Classpath`] from project-module references.
///
/// Each `(scope, module_path)` pair is resolved against `module_outputs`
/// (a map from module relative path to output artifact) and
/// `module_api_classpaths` (direct `api`-scoped jars per module).
///
/// `implementation` and `api` refs inject the module output into both
/// compile and runtime.  `api` refs additionally propagate the depended
/// module's `api` classpath (direct `api`-scoped jars) so that consumers
/// see transitive `api` deps.  `runtimeOnly` injects into runtime only;
/// `compileOnly` injects into compile only; `testImplementation` injects
/// into test compile and test runtime.
///
/// `ksp` and `androidTestImplementation` have no cross-module meaning for
/// project references; a ref using either scope is reported on stderr as
/// a warning and skipped, degrading to a smaller classpath rather than
/// failing the build.
///
/// # Errors
///
/// Returns an error when a referenced module path does not appear in
/// `settings_modules` or has no discoverable output.
fn resolve_project_classpath(
    project_refs: &[(maven::MavenScope, String)],
    module_outputs: &std::collections::HashMap<String, PathBuf>,
    module_api_classpaths: &std::collections::HashMap<String, Vec<PathBuf>>,
    settings_modules: &[String],
) -> Result<maven::Classpath, String> {
    let mut classpath = maven::Classpath::default();
    for (scope, module_path) in project_refs {
        // Strip the leading ':' — settings modules are declared without it.
        let module_name = module_path.strip_prefix(':').unwrap_or(module_path);
        if !settings_modules.iter().any(|m| m == module_name) {
            return Err(format!(
                "project(\"{module_path}\"): module '{module_name}' is not declared in settings.ulb"
            ));
        }
        let output = module_outputs.get(module_name).ok_or_else(|| {
            format!(
                "project(\"{module_path}\"): module '{module_name}' has no discoverable output \
                 (expected jvm.jarFile or android.apk in its build.ulb)"
            )
        })?;
        match scope {
            maven::MavenScope::Api => {
                classpath.compile.push(output.clone());
                classpath.runtime.push(output.clone());
                if let Some(api_jars) = module_api_classpaths.get(module_name) {
                    for jar in api_jars {
                        if !classpath.compile.contains(jar) {
                            classpath.compile.push(jar.clone());
                        }
                        if !classpath.runtime.contains(jar) {
                            classpath.runtime.push(jar.clone());
                        }
                    }
                }
            }
            maven::MavenScope::Implementation => {
                classpath.compile.push(output.clone());
                classpath.runtime.push(output.clone());
            }
            maven::MavenScope::RuntimeOnly => {
                classpath.runtime.push(output.clone());
            }
            maven::MavenScope::CompileOnly => {
                classpath.compile.push(output.clone());
            }
            maven::MavenScope::TestImplementation => {
                classpath.test_compile.push(output.clone());
                classpath.test_runtime.push(output.clone());
            }
            maven::MavenScope::Ksp | maven::MavenScope::AndroidTestImplementation => {
                // Only these two scopes reach this arm.
                let scope_name = match scope {
                    maven::MavenScope::Ksp => "ksp",
                    _ => "androidTestImplementation",
                };
                eprintln!(
                    "warning: project(\"{module_path}\"): {scope_name} scope is not supported \
                     for project dependencies; skipping"
                );
            }
        }
    }
    Ok(classpath)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_to_value(json: &serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Invalid("null".to_owned()),
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Number(Number::Int(i))
                } else if let Some(f) = n.as_f64() {
                    Value::Number(Number::Float(f))
                } else {
                    Value::Invalid("invalid number".to_owned())
                }
            }
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(arr) => Value::List(arr.iter().map(json_to_value).collect()),
            serde_json::Value::Object(map) => {
                let entries: std::collections::BTreeMap<String, Value> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), json_to_value(v)))
                    .collect();
                Value::Block(entries)
            }
        }
    }

    #[test]
    fn project_dir_is_canonicalized_to_an_absolute_path() {
        let base = std::env::temp_dir().join(format!("uliab-canonical-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("proj")).expect("temp project");
        let resolved = canonical_project_dir(&base.join("proj")).expect("resolves");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, base.join("proj"));
        let error = canonical_project_dir(&base.join("missing")).expect_err("missing project");
        assert!(
            error.contains("cannot resolve project directory"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

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

    #[test]
    fn sdk_candidates_lead_with_the_override_and_end_with_the_home_fallback() {
        let candidates = sdk_candidates(
            Some(Path::new("/opt/sdk")),
            Some(Path::new("/home/example")),
        );
        assert_eq!(candidates.first().unwrap(), &PathBuf::from("/opt/sdk"));
        assert_eq!(
            candidates.last().unwrap(),
            &PathBuf::from("/home/example/Android/Sdk")
        );
        // The override always leads, so it wins over any environment or
        // home candidate no matter what the machine has set.
        let no_override = sdk_candidates(None, Some(Path::new("/home/example")));
        assert_eq!(
            no_override.last().unwrap(),
            &PathBuf::from("/home/example/Android/Sdk")
        );
    }

    #[test]
    fn first_existing_sdk_dir_skips_missing_candidates() {
        let base =
            std::env::temp_dir().join(format!("uliab-sdk-candidates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real")).expect("create temp dir");
        let candidates = vec![
            base.join("missing"),
            base.join("real"),
            base.join("also-missing"),
        ];
        assert_eq!(first_existing_sdk_dir(&candidates), Some(base.join("real")));
        assert_eq!(first_existing_sdk_dir(&[base.join("missing")]), None);
    }

    #[test]
    fn android_sdk_root_accepts_an_existing_override() {
        let base = std::env::temp_dir().join(format!("uliab-sdk-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sdk")).expect("create temp dir");
        assert_eq!(
            android_sdk_root(Some(&base.join("sdk"))),
            Some(base.join("sdk"))
        );
    }

    #[test]
    fn an_explicit_override_that_does_not_exist_is_an_error() {
        let base = std::env::temp_dir().join(format!("uliab-sdk-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp dir");
        let missing = base.join("does-not-exist");
        let error = checked_sdk_root(Some(&missing)).expect_err("missing override");
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains("must name an existing directory"), "{error}");
    }

    #[test]
    fn an_existing_override_wins_without_probing_the_environment() {
        // The override is returned as-is even though this machine's real
        // `~/Android/Sdk` would otherwise resolve; the explicit path must
        // not be silently replaced by an environment convention.
        let base = std::env::temp_dir().join(format!("uliab-sdk-override-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("custom")).expect("create temp dir");
        assert_eq!(
            checked_sdk_root(Some(&base.join("custom"))).expect("existing override"),
            Some(base.join("custom"))
        );
    }

    #[test]
    fn a_module_sdk_dir_is_resolved_against_the_project_dir_when_relative() {
        let project = Path::new("/tmp/project");
        let config = serde_json::json!({
            "source": "in.txt",
            "android": { "sdkDir": "local/sdk" },
        });
        assert_eq!(
            module_android_sdk_dir(&config, project),
            Some(PathBuf::from("/tmp/project/local/sdk"))
        );
    }

    #[test]
    fn an_absolute_module_sdk_dir_is_left_untouched() {
        let config = serde_json::json!({
            "android": { "sdkDir": "/opt/android-sdk" },
        });
        assert_eq!(
            module_android_sdk_dir(&config, Path::new("/tmp/project")),
            Some(PathBuf::from("/opt/android-sdk"))
        );
    }

    #[test]
    fn a_module_without_a_string_sdk_dir_declares_none() {
        assert_eq!(
            module_android_sdk_dir(&serde_json::json!({ "android": {} }), Path::new("/p")),
            None
        );
        assert_eq!(
            module_android_sdk_dir(
                &serde_json::json!({ "android": { "compileSdk": 36 } }),
                Path::new("/p")
            ),
            None
        );
        assert_eq!(
            module_android_sdk_dir(&serde_json::json!({}), Path::new("/p")),
            None
        );
    }

    #[test]
    fn source_set_deps_are_collected_by_key_path() {
        let block = |entries: &[(&str, Value)]| -> Value {
            Value::Block(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            )
        };
        let deps_block = block(&[(
            "implementation",
            Value::List(vec![Value::Str("com.example:one:1.0".to_owned())]),
        )]);
        let model = block(&[
            ("deps", deps_block.clone()),
            ("commonMain", block(&[("deps", deps_block.clone())])),
            (
                "android",
                block(&[("compileSdk", Value::Number(Number::Int(35)))]),
            ),
            (
                "kmp",
                block(&[("commonMain", block(&[("deps", deps_block)]))]),
            ),
        ]);

        let mut found = Vec::new();
        let Value::Block(top) = &model else {
            panic!("model is a block");
        };
        for (key, value) in top {
            collect_source_set_deps(&mut found, &mut vec![key.clone()], value).expect("collects");
        }
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["commonMain", "kmp.commonMain"]);
    }

    #[test]
    fn a_non_block_source_set_deps_names_its_path() {
        let model = Value::Block(
            [(
                "commonMain".to_owned(),
                Value::Block(
                    [("deps".to_owned(), Value::Str("x".to_owned()))]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let error = resolve_source_set_classpaths(&model, &[], None).expect_err("malformed deps");
        assert!(error.contains("deps at 'commonMain'"), "{error}");
    }

    #[test]
    fn source_set_project_refs_are_collected_by_key_path_and_scope() {
        let block = |entries: &[(&str, Value)]| -> Value {
            Value::Block(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            )
        };
        let model = block(&[
            (
                "deps",
                block(&[("implementation", Value::ProjectRef(":top".to_owned()))]),
            ),
            (
                "commonMain",
                block(&[(
                    "deps",
                    block(&[
                        ("api", Value::ProjectRef(":lib".to_owned())),
                        (
                            "implementation",
                            Value::Str("com.example:one:1.0".to_owned()),
                        ),
                    ]),
                )]),
            ),
            (
                "jvmTest",
                block(&[(
                    "deps",
                    block(&[(
                        "testImplementation",
                        Value::List(vec![Value::ProjectRef(":testing".to_owned())]),
                    )]),
                )]),
            ),
        ]);

        let refs = collect_source_set_project_refs(&model).expect("collects");
        assert_eq!(refs.len(), 2, "top-level deps are not source sets");
        assert_eq!(
            refs[0],
            (
                "commonMain".to_owned(),
                vec![(maven::MavenScope::Api, ":lib".to_owned())]
            )
        );
        assert_eq!(
            refs[1],
            (
                "jvmTest".to_owned(),
                vec![(maven::MavenScope::TestImplementation, ":testing".to_owned())]
            )
        );
    }

    #[test]
    fn reject_project_refs_names_top_level_and_nested_locations() {
        let block = |entries: &[(&str, Value)]| -> Value {
            Value::Block(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            )
        };
        let clean = block(&[
            (
                "jvm",
                block(&[("jarFile", Value::Str("build/app.jar".to_owned()))]),
            ),
            (
                "commonMain",
                block(&[(
                    "deps",
                    block(&[(
                        "implementation",
                        Value::Str("com.example:one:1.0".to_owned()),
                    )]),
                )]),
            ),
        ]);
        assert!(reject_project_refs(&clean).is_ok());

        let offending = block(&[
            (
                "deps",
                block(&[("api", Value::ProjectRef(":lib".to_owned()))]),
            ),
            (
                "kmp",
                block(&[(
                    "commonMain",
                    block(&[(
                        "deps",
                        block(&[("api", Value::ProjectRef(":lib".to_owned()))]),
                    )]),
                )]),
            ),
        ]);
        let error = reject_project_refs(&offending).expect_err("project refs rejected");
        assert!(error.contains("settings.ulb"), "{error}");
        assert!(error.contains("deps"), "{error}");
        assert!(error.contains("kmp.commonMain.deps"), "{error}");
    }

    #[test]
    fn discover_module_output_finds_jvm_jar() {
        let model = Value::Block(
            [(
                "jvm".to_owned(),
                Value::Block(
                    [("jarFile".to_owned(), Value::Str("build/app.jar".to_owned()))]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let output = discover_module_output(&model, Path::new("/project/app"));
        assert_eq!(output, Some(PathBuf::from("/project/app/build/app.jar")));
    }

    #[test]
    fn discover_module_output_finds_android_apk() {
        let model = Value::Block(
            [(
                "android".to_owned(),
                Value::Block(
                    [("apk".to_owned(), Value::Str("build/app.apk".to_owned()))]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let output = discover_module_output(&model, Path::new("/project/app"));
        assert_eq!(output, Some(PathBuf::from("/project/app/build/app.apk")));
    }

    #[test]
    fn discover_module_output_returns_none_for_unknown_plugin() {
        let model = Value::Block(
            [(
                "web".to_owned(),
                Value::Block(
                    [("out".to_owned(), Value::Str("dist/index.html".to_owned()))]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(discover_module_output(&model, Path::new("/project")), None);
    }

    #[test]
    fn discover_module_output_returns_none_for_non_block() {
        assert_eq!(
            discover_module_output(&Value::Str("not a block".to_owned()), Path::new("/p")),
            None
        );
    }

    #[test]
    fn resolve_project_classpath_implementation_injects_compile_and_runtime() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "shared".to_owned(),
            PathBuf::from("/project/shared/build/app.jar"),
        );
        let modules = vec!["shared".to_owned()];
        let api_cp = std::collections::HashMap::new();
        let refs = vec![(maven::MavenScope::Implementation, ":shared".to_owned())];
        let cp = resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect("resolves");
        assert_eq!(cp.compile.len(), 1);
        assert_eq!(cp.runtime.len(), 1);
        assert_eq!(cp.test_compile.len(), 0);
    }

    #[test]
    fn resolve_project_classpath_runtime_only_injects_runtime_only() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "lib".to_owned(),
            PathBuf::from("/project/lib/build/app.jar"),
        );
        let modules = vec!["lib".to_owned()];
        let api_cp = std::collections::HashMap::new();
        let refs = vec![(maven::MavenScope::RuntimeOnly, ":lib".to_owned())];
        let cp = resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect("resolves");
        assert_eq!(cp.compile.len(), 0);
        assert_eq!(cp.runtime.len(), 1);
    }

    #[test]
    fn resolve_project_classpath_unknown_module_is_error() {
        let outputs = std::collections::HashMap::new();
        let modules = vec!["app".to_owned()];
        let api_cp = std::collections::HashMap::new();
        let refs = vec![(maven::MavenScope::Implementation, ":missing".to_owned())];
        let error =
            resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect_err("missing");
        assert!(error.contains("not declared in settings"), "{error}");
    }

    #[test]
    fn resolve_project_classpath_module_without_output_is_error() {
        let outputs = std::collections::HashMap::new();
        let modules = vec!["shared".to_owned()];
        let api_cp = std::collections::HashMap::new();
        let refs = vec![(maven::MavenScope::Implementation, ":shared".to_owned())];
        let error =
            resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect_err("no output");
        assert!(error.contains("no discoverable output"), "{error}");
    }

    #[test]
    fn resolve_project_classpath_test_impl_injects_test_buckets() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "testlib".to_owned(),
            PathBuf::from("/project/testlib/build/app.jar"),
        );
        let modules = vec!["testlib".to_owned()];
        let api_cp = std::collections::HashMap::new();
        let refs = vec![(maven::MavenScope::TestImplementation, ":testlib".to_owned())];
        let cp = resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect("resolves");
        assert_eq!(cp.compile.len(), 0);
        assert_eq!(cp.runtime.len(), 0);
        assert_eq!(cp.test_compile.len(), 1);
        assert_eq!(cp.test_runtime.len(), 1);
    }

    #[test]
    fn resolve_project_classpath_api_propagates_dep_api_classpath() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "shared".to_owned(),
            PathBuf::from("/project/shared/build/app.jar"),
        );
        let mut api_cp = std::collections::HashMap::new();
        api_cp.insert(
            "shared".to_owned(),
            vec![
                PathBuf::from("/repo/com/example/one/1.0/one-1.0.jar"),
                PathBuf::from("/repo/com/example/two/2.0/two-2.0.jar"),
            ],
        );
        let modules = vec!["shared".to_owned()];
        let refs = vec![(maven::MavenScope::Api, ":shared".to_owned())];
        let cp = resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect("resolves");
        assert_eq!(cp.compile.len(), 3, "output + 2 api jars");
        assert_eq!(cp.runtime.len(), 3, "output + 2 api jars");
        assert!(
            cp.compile
                .contains(&PathBuf::from("/project/shared/build/app.jar"))
        );
        assert!(
            cp.compile
                .contains(&PathBuf::from("/repo/com/example/one/1.0/one-1.0.jar"))
        );
        assert!(
            cp.compile
                .contains(&PathBuf::from("/repo/com/example/two/2.0/two-2.0.jar"))
        );
    }

    #[test]
    fn resolve_project_classpath_implementation_does_not_propagate_api_classpath() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "shared".to_owned(),
            PathBuf::from("/project/shared/build/app.jar"),
        );
        let mut api_cp = std::collections::HashMap::new();
        api_cp.insert(
            "shared".to_owned(),
            vec![PathBuf::from("/repo/com/example/one/1.0/one-1.0.jar")],
        );
        let modules = vec!["shared".to_owned()];
        let refs = vec![(maven::MavenScope::Implementation, ":shared".to_owned())];
        let cp = resolve_project_classpath(&refs, &outputs, &api_cp, &modules).expect("resolves");
        assert_eq!(cp.compile.len(), 1, "only output, no api jars");
        assert_eq!(cp.runtime.len(), 1, "only output, no api jars");
    }

    #[test]
    fn compose_deps_returns_bom_and_standard_artifacts() {
        let model = serde_json::json!({
            "android": {
                "compose": true,
                "composeVersion": "3.1.0"
            }
        });
        let value = json_to_value(&model);
        let deps = compose_deps(&value)
            .expect("resolves")
            .expect("compose is true");
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].dependency.group, "androidx.compose");
        assert_eq!(deps[0].dependency.artifact, "compose-bom");
        assert_eq!(deps[0].dependency.version, "3.1.0");
        assert_eq!(deps[1].dependency.group, "androidx.compose.runtime");
        assert_eq!(deps[1].dependency.artifact, "runtime");
        assert!(deps[1].dependency.is_version_managed());
        assert_eq!(deps[2].dependency.group, "androidx.compose.ui");
        assert_eq!(deps[2].dependency.artifact, "ui");
        assert!(deps[2].dependency.is_version_managed());
        assert_eq!(deps[3].dependency.group, "androidx.compose.material3");
        assert_eq!(deps[3].dependency.artifact, "material3");
        assert!(deps[3].dependency.is_version_managed());
    }

    #[test]
    fn compose_deps_uses_default_version_when_no_compose_version() {
        let model = serde_json::json!({
            "android": {
                "compose": true
            }
        });
        let value = json_to_value(&model);
        let deps = compose_deps(&value)
            .expect("resolves")
            .expect("compose is true");
        assert_eq!(deps[0].dependency.version, DEFAULT_COMPOSE_BOM_VERSION);
    }

    #[test]
    fn compose_deps_returns_none_when_compose_false() {
        let model = serde_json::json!({
            "android": {
                "compose": false
            }
        });
        let value = json_to_value(&model);
        assert!(compose_deps(&value).expect("resolves").is_none());
    }

    #[test]
    fn compose_deps_returns_none_when_no_android_block() {
        let model = serde_json::json!({
            "jvm": {}
        });
        let value = json_to_value(&model);
        assert!(compose_deps(&value).expect("resolves").is_none());
    }

    #[test]
    fn source_set_classpath_injects_compose_deps_when_compose_true() {
        let tmp = std::env::temp_dir().join(format!("uliab-compose-ss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("repo");
        let cache_dir = tmp.join("cache");
        let pom_dir = repo_dir.join("com/example/lib/1.0");
        std::fs::create_dir_all(&pom_dir).expect("create repo");
        std::fs::write(
            pom_dir.join("lib-1.0.pom"),
            "<?xml version=\"1.0\"?><project>\
             <modelVersion>4.0.0</modelVersion>\
             <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>\
             </project>",
        )
        .expect("write pom");
        std::fs::write(pom_dir.join("lib-1.0.jar"), b"jar").expect("write jar");

        let model = Value::Block(
            [
                (
                    "android".to_owned(),
                    Value::Block(
                        [("compose".to_owned(), Value::Bool(true))]
                            .into_iter()
                            .collect(),
                    ),
                ),
                (
                    "commonMain".to_owned(),
                    Value::Block(
                        [(
                            "deps".to_owned(),
                            Value::Block(
                                [(
                                    "implementation".to_owned(),
                                    Value::Str("com.example:lib:1.0".to_owned()),
                                )]
                                .into_iter()
                                .collect(),
                            ),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let repos = vec![maven::MavenRepo::Custom(
            repo_dir.to_string_lossy().into_owned(),
        )];
        let result = resolve_source_set_classpaths(&model, &repos, Some(cache_dir));
        let error = result.expect_err("BOM resolution fails with local-only repo");
        assert!(
            error.contains("compose-bom"),
            "error should mention the BOM: {error}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn source_set_classpath_no_compose_injection_when_compose_false() {
        let tmp =
            std::env::temp_dir().join(format!("uliab-compose-ss-false-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("repo");
        let cache_dir = tmp.join("cache");
        let pom_dir = repo_dir.join("com/example/lib/1.0");
        std::fs::create_dir_all(&pom_dir).expect("create repo");
        std::fs::write(
            pom_dir.join("lib-1.0.pom"),
            "<?xml version=\"1.0\"?><project>\
             <modelVersion>4.0.0</modelVersion>\
             <groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version>\
             </project>",
        )
        .expect("write pom");
        std::fs::write(pom_dir.join("lib-1.0.jar"), b"jar").expect("write jar");

        let model = Value::Block(
            [
                (
                    "android".to_owned(),
                    Value::Block(
                        [("compose".to_owned(), Value::Bool(false))]
                            .into_iter()
                            .collect(),
                    ),
                ),
                (
                    "commonMain".to_owned(),
                    Value::Block(
                        [(
                            "deps".to_owned(),
                            Value::Block(
                                [(
                                    "implementation".to_owned(),
                                    Value::Str("com.example:lib:1.0".to_owned()),
                                )]
                                .into_iter()
                                .collect(),
                            ),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let repos = vec![maven::MavenRepo::Custom(
            repo_dir.to_string_lossy().into_owned(),
        )];
        let resolved = resolve_source_set_classpaths(&model, &repos, Some(cache_dir))
            .expect("resolves without compose");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "commonMain");
        assert_eq!(resolved[0].1.compile.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn source_set_classpath_compose_resolves_bom_and_managed_deps() {
        let tmp = std::env::temp_dir().join(format!("uliab-compose-ss-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo_dir = tmp.join("repo");
        let cache_dir = tmp.join("cache");

        let bom_dir = repo_dir.join("androidx/compose/compose-bom/2026.08.00");
        std::fs::create_dir_all(&bom_dir).expect("create bom dir");
        std::fs::write(
            bom_dir.join("compose-bom-2026.08.00.pom"),
            "<?xml version=\"1.0\"?><project>\
             <modelVersion>4.0.0</modelVersion>\
             <groupId>androidx.compose</groupId>\
             <artifactId>compose-bom</artifactId>\
             <version>2026.08.00</version>\
             <packaging>pom</packaging>\
             <dependencyManagement><dependencies>\
               <dependency><groupId>androidx.compose.runtime</groupId>\
                 <artifactId>runtime</artifactId><version>1.0</version></dependency>\
               <dependency><groupId>androidx.compose.ui</groupId>\
                 <artifactId>ui</artifactId><version>1.0</version></dependency>\
               <dependency><groupId>androidx.compose.material3</groupId>\
                 <artifactId>material3</artifactId><version>1.0</version></dependency>\
             </dependencies></dependencyManagement>\
             </project>",
        )
        .expect("write bom pom");

        for (group_path, artifact) in [
            ("androidx/compose/runtime", "runtime"),
            ("androidx/compose/ui", "ui"),
            ("androidx/compose/material3", "material3"),
        ] {
            let dir = repo_dir.join(group_path).join(artifact).join("1.0");
            std::fs::create_dir_all(&dir).expect("create dep dir");
            std::fs::write(
                dir.join(format!("{artifact}-1.0.pom")),
                format!(
                    "<?xml version=\"1.0\"?><project>\
                     <modelVersion>4.0.0</modelVersion>\
                     <groupId>androidx.compose.{artifact}</groupId>\
                     <artifactId>{artifact}</artifactId><version>1.0</version>\
                     </project>"
                ),
            )
            .expect("write pom");
            std::fs::write(dir.join(format!("{artifact}-1.0.jar")), b"jar").expect("write jar");
        }

        let model = Value::Block(
            [
                (
                    "android".to_owned(),
                    Value::Block(
                        [("compose".to_owned(), Value::Bool(true))]
                            .into_iter()
                            .collect(),
                    ),
                ),
                (
                    "commonMain".to_owned(),
                    Value::Block(
                        [("deps".to_owned(), Value::Block([].into_iter().collect()))]
                            .into_iter()
                            .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let repos = vec![maven::MavenRepo::Custom(
            repo_dir.to_string_lossy().into_owned(),
        )];
        let resolved = resolve_source_set_classpaths(&model, &repos, Some(cache_dir))
            .expect("resolves with full compose BOM repo");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "commonMain");
        let compile_jars: Vec<&str> = resolved[0]
            .1
            .compile
            .iter()
            .filter_map(|p| p.file_name().map(|f| f.to_str().unwrap()))
            .collect();
        assert!(
            compile_jars.iter().any(|j| j.contains("runtime-1.0")),
            "managed runtime should be on compile classpath: {compile_jars:?}"
        );
        assert!(
            compile_jars.iter().any(|j| j.contains("ui-1.0")),
            "managed ui should be on compile classpath: {compile_jars:?}"
        );
        assert!(
            compile_jars.iter().any(|j| j.contains("material3-1.0")),
            "managed material3 should be on compile classpath: {compile_jars:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
