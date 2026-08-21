//! Project-file loading for the tool layer (ARCHITECTURE.md §9, step 2).
//!
//! The registry client only needs the plugin table of a project's
//! `libs.ulb`: the `plugins { NAME = "vendor/name" @ refOrString }`
//! declarations (GRAMMAR.md §6.4). This module parses that file through the
//! [`ulb_lang`] pipeline and lifts each entry into a [`PluginSpec`].

use std::path::{Path, PathBuf};

use ulb_lang::eval::{Definitions, SettingsModel};
use ulb_lang::parse;

use crate::registry::PluginSpec;

/// The `plugins {}` table lifted from a project's `libs.ulb`.
pub struct ProjectPlugins {
    /// Path of the `libs.ulb` the entries came from.
    pub libs_path: PathBuf,
    /// One spec per `plugins {}` entry, in declaration order as stored in
    /// the evaluator's [`Definitions`] (sorted by alias name).
    pub plugins: Vec<PluginSpec>,
}

/// Reads `<dir>/libs.ulb` and extracts its `plugins {}` declarations.
///
/// The file is parsed and run through [`ulb_lang::eval::collect_definitions`],
/// so `@` version references against `versions {}` resolve exactly as they
/// do for the evaluator, and a duplicate-version or unknown-reference error
/// surfaces as a diagnostic rather than a silent misparse.
///
/// # Errors
///
/// Returns an error when `dir` has no `libs.ulb`, when the file has parse
/// errors (the entries would be untrustworthy), or when an entry is not a
/// plugin reference.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use std::path::PathBuf;
/// use uliab::project::read_libs_plugins;
///
/// let dir = std::env::temp_dir().join(format!(
///     "uliab-project-test-{}",
///     std::process::id()
/// ));
/// fs::create_dir_all(&dir).unwrap();
/// fs::write(
///     dir.join("libs.ulb"),
///     "plugins {\n  hello = \"ulite/hello\" @ \"0.1.0\"\n}\n",
/// )
/// .unwrap();
///
/// let project = read_libs_plugins(&dir).expect("reads libs.ulb");
/// assert_eq!(project.plugins.len(), 1);
/// assert_eq!(project.plugins[0].name, "ulite/hello");
/// assert_eq!(project.plugins[0].version.as_deref(), Some("0.1.0"));
///
/// let _ = fs::remove_dir_all(&dir);
/// ```
pub fn read_libs_plugins(dir: &Path) -> Result<ProjectPlugins, String> {
    let libs_path = dir.join("libs.ulb");
    let source = std::fs::read_to_string(&libs_path)
        .map_err(|error| format!("no libs.ulb in {}: {error}", dir.display()))?;

    let parsed = parse(&source);
    if !parsed.diagnostics.is_empty() {
        let messages = parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "{} has parse errors: {messages}",
            libs_path.display()
        ));
    }

    let mut defs = Definitions::default();
    let mut diagnostics = Vec::new();
    ulb_lang::eval::collect_definitions(&parsed.file, &mut defs, &mut diagnostics);
    if !diagnostics.is_empty() {
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "{} has evaluation errors: {messages}",
            libs_path.display()
        ));
    }

    let mut plugins = Vec::with_capacity(defs.plugins.len());
    for (alias, value) in &defs.plugins {
        match PluginSpec::from_value(value) {
            Ok(spec) => plugins.push(spec),
            Err(message) => {
                return Err(format!(
                    "{}: plugin '{}': {message}",
                    libs_path.display(),
                    alias
                ));
            }
        }
    }

    Ok(ProjectPlugins { libs_path, plugins })
}

/// Renders a spec for CLI output, e.g. `ulite/hello (unversioned)`.
#[must_use]
pub fn spec_label(spec: &PluginSpec) -> String {
    match &spec.version {
        Some(version) => format!("{}@{}", spec.name, version),
        None => format!("{} (newest compatible)", spec.name),
    }
}

/// Resolved project settings from `settings.ulb`.
///
/// Wraps the evaluator's [`SettingsModel`] with resolved absolute module
/// directories so the driver can iterate modules without re-resolving paths.
pub struct ProjectSettings {
    /// The raw evaluator model (project name, module paths, extra repos,
    /// lspCompat flag).
    pub model: SettingsModel,
    /// Absolute paths to each module's root directory, derived by joining
    /// the project root with each `module "path"` declaration.
    pub module_dirs: Vec<PathBuf>,
    /// The project root directory these settings were read from.
    pub project_dir: PathBuf,
}

/// Reads `<dir>/settings.ulb` and evaluates it into a [`ProjectSettings`].
///
/// Returns `Ok(None)` when the file does not exist — a project without
/// `settings.ulb` is a single-module project, not an error. Returns errors
/// for parse failures, evaluation diagnostics, or missing `module`
/// declarations in a file that exists.
///
/// # Errors
///
/// Returns an error when `settings.ulb` exists but cannot be read, has parse
/// or evaluation errors, or declares no modules.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use std::path::PathBuf;
/// use uliab::project::read_settings;
///
/// let dir = std::env::temp_dir().join(format!(
///     "uliab-settings-test-{}", std::process::id()
/// ));
/// fs::create_dir_all(&dir).unwrap();
/// fs::write(
///     dir.join("settings.ulb"),
///     "project \"MyApp\"\nmodule \"app\"\nmodule \"shared\"\n",
/// )
/// .unwrap();
///
/// let settings = read_settings(&dir).expect("reads settings.ulb").expect("settings exist");
/// assert_eq!(settings.model.project_name.as_deref(), Some("MyApp"));
/// assert_eq!(settings.model.modules, vec!["app", "shared"]);
/// assert_eq!(settings.module_dirs.len(), 2);
/// assert!(settings.module_dirs[0].ends_with("app"));
/// assert!(settings.module_dirs[1].ends_with("shared"));
///
/// let _ = fs::remove_dir_all(&dir);
/// ```
pub fn read_settings(dir: &Path) -> Result<Option<ProjectSettings>, String> {
    let settings_path = dir.join("settings.ulb");
    let source = match std::fs::read_to_string(&settings_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("reading {}: {error}", settings_path.display()));
        }
    };

    let outcome = ulb_lang::eval::evaluate_settings(&source);
    if !outcome.diagnostics.is_empty() {
        let messages = outcome
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("{}: {messages}", settings_path.display()));
    }

    if outcome.model.modules.is_empty() {
        return Err(format!(
            "{} declares no modules; add at least one 'module \"path\"' declaration",
            settings_path.display()
        ));
    }

    let mut module_dirs = Vec::with_capacity(outcome.model.modules.len());
    for path in &outcome.model.modules {
        if std::path::Path::new(path).is_absolute() {
            return Err(format!(
                "module '{path}' must be a relative path (no leading '/')"
            ));
        }
        if path.contains("..") {
            return Err(format!("module '{path}' must not contain '..' segments"));
        }
        module_dirs.push(dir.join(path));
    }

    Ok(Some(ProjectSettings {
        model: outcome.model,
        module_dirs,
        project_dir: dir.to_path_buf(),
    }))
}
