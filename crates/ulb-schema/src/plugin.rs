//! Plugin identity extraction and on-disk schema discovery.
//!
//! A `libs.ulb` `plugins {}` table maps a plugin alias to a registry
//! coordinate. Both the `uliab` host and the `ulb-lsp` editor integration
//! must agree on how such a value maps to a concrete `(name, version)` and
//! then to the resolved plugin's cached `.wasm` on disk, because that file
//! is what carries the embedded config schema this crate extracts. Keeping
//! that mapping here (rather than duplicated in each consumer) is what stops
//! the two from drifting apart.

use std::path::{Path, PathBuf};

use ulb_lang::eval::Value;

/// A plugin reference extracted from a `libs.ulb` `plugins {}` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    /// Plugin name as it resolves in the registry, e.g. `ulite/hello`.
    pub name: String,
    /// Pinned version, or `None` to resolve the newest compatible build.
    pub version: Option<String>,
}

impl PluginSpec {
    /// Extracts a spec from the value the evaluator produced for one
    /// `plugins { NAME = ... }` entry: a `"vendor/name" @ ref` reference
    /// becomes a [`Value::Coordinate`] (`"name:version"`), an unversioned
    /// one a [`Value::Str`].
    ///
    /// # Errors
    ///
    /// Returns a description of the problem when the value is not a plugin
    /// reference (a non-string base, an invalid value, or a malformed
    /// coordinate).
    ///
    /// # Examples
    ///
    /// ```
    /// use ulb_schema::plugin::PluginSpec;
    /// use ulb_lang::eval::Value;
    ///
    /// let versioned =
    ///     PluginSpec::from_value(&Value::Coordinate("ulite/hello:0.1.0".to_owned()))
    ///         .expect("valid coordinate");
    /// assert_eq!(versioned.name, "ulite/hello");
    /// assert_eq!(versioned.version.as_deref(), Some("0.1.0"));
    ///
    /// let unversioned =
    ///     PluginSpec::from_value(&Value::Str("ulite/hello".to_owned()))
    ///         .expect("valid name");
    /// assert_eq!(unversioned.version, None);
    ///
    /// assert!(PluginSpec::from_value(&Value::Bool(true)).is_err());
    /// ```
    pub fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Coordinate(coordinate) => match coordinate.split_once(':') {
                Some((name, version)) => Ok(Self {
                    name: name.to_owned(),
                    version: Some(version.to_owned()),
                }),
                None => Err(format!(
                    "malformed plugin coordinate '{coordinate}' (expected 'name:version')"
                )),
            },
            Value::Str(name) => Ok(Self {
                name: name.clone(),
                version: None,
            }),
            Value::Invalid(message) => Err(message.clone()),
            other => Err(format!(
                "plugin reference must be a string coordinate, got {}",
                value_kind(other)
            )),
        }
    }

    /// The path to the plugin's cached `.wasm` under `cache_dir`, or `None`
    /// when the spec pins no version (an unpinned spec has no concrete
    /// artifact to point at).
    ///
    /// This mirrors the layout the host registrar reads on a cache hit:
    /// `<cache_dir>/<name>/<version>/plugin.wasm`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use ulb_schema::plugin::PluginSpec;
    /// use ulb_lang::eval::Value;
    ///
    /// let versioned =
    ///     PluginSpec::from_value(&Value::Coordinate("ulite/hello:0.1.0".to_owned())).unwrap();
    /// assert_eq!(
    ///     versioned.cache_wasm_path(Path::new("/home/u/.cache/uliab/plugins")),
    ///     Some(Path::new("/home/u/.cache/uliab/plugins/ulite/hello/0.1.0/plugin.wasm").to_path_buf())
    /// );
    ///
    /// let unversioned = PluginSpec::from_value(&Value::Str("ulite/hello".to_owned())).unwrap();
    /// assert_eq!(unversioned.cache_wasm_path(Path::new("/cache")), None);
    /// ```
    pub fn cache_wasm_path(&self, cache_dir: &Path) -> Option<PathBuf> {
        let version = self.version.as_ref()?;
        Some(cache_dir.join(&self.name).join(version).join("plugin.wasm"))
    }
}

impl std::fmt::Display for PluginSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.version {
            Some(version) => write!(f, "{}@{}", self.name, version),
            None => write!(f, "{}", self.name),
        }
    }
}

/// The default directory the registry caches downloaded plugin artifacts
/// under: `$HOME/.cache/uliab/plugins` (falling back to a relative path when
/// `$HOME` is unset).
pub fn default_plugins_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".cache")
            .join("uliab")
            .join("plugins"),
        None => PathBuf::from(".cache").join("uliab").join("plugins"),
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Str(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Version(_) => "version",
        Value::Properties(_) => "properties",
        Value::Coordinate(_) => "coordinate",
        Value::Block(_) => "block",
        Value::Invalid(_) => "invalid",
        Value::ProjectRef(_) => "project reference",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_value_accepts_string_name_and_coordinate() {
        let name = PluginSpec::from_value(&Value::Str("ulite/hello".to_owned())).unwrap();
        assert_eq!(name.name, "ulite/hello");
        assert_eq!(name.version, None);

        let versioned =
            PluginSpec::from_value(&Value::Coordinate("ulite/hello:0.2.0".to_owned())).unwrap();
        assert_eq!(versioned.name, "ulite/hello");
        assert_eq!(versioned.version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn from_value_rejects_non_plugin_values() {
        use ulb_lang::token::Number;
        assert!(PluginSpec::from_value(&Value::Bool(true)).is_err());
        assert!(PluginSpec::from_value(&Value::Number(Number::Int(1))).is_err());
        assert!(PluginSpec::from_value(&Value::List(vec![])).is_err());
    }

    #[test]
    fn from_value_rejects_coordinate_without_version() {
        let error =
            PluginSpec::from_value(&Value::Coordinate("ulite/hello".to_owned())).unwrap_err();
        assert!(error.contains("malformed plugin coordinate"));
    }

    #[test]
    fn cache_wasm_path_combines_name_version_and_filename() {
        let spec =
            PluginSpec::from_value(&Value::Coordinate("ulite/hello:0.1.0".to_owned())).unwrap();
        let path = spec
            .cache_wasm_path(Path::new("/cache"))
            .expect("versioned spec has a path");
        assert_eq!(path, PathBuf::from("/cache/ulite/hello/0.1.0/plugin.wasm"));
    }

    #[test]
    fn cache_wasm_path_is_none_for_an_unversioned_spec() {
        let spec = PluginSpec::from_value(&Value::Str("ulite/hello".to_owned())).unwrap();
        assert_eq!(spec.cache_wasm_path(Path::new("/cache")), None);
    }
}
