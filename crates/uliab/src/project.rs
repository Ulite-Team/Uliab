//! Project-file loading for the tool layer (ARCHITECTURE.md §9, step 2).
//!
//! The registry client only needs the plugin table of a project's
//! `libs.ulb`: the `plugins { NAME = "vendor/name" @ refOrString }`
//! declarations (GRAMMAR.md §6.4). This module parses that file through the
//! [`ulb_lang`] pipeline and lifts each entry into a [`PluginSpec`].

use std::path::{Path, PathBuf};

use ulb_lang::eval::Definitions;
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
