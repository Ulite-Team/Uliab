//! Plugin registry resolution (ARCHITECTURE.md §3.6, §9 step 6).
//!
//! A plugin coordinate declared in a `libs.ulb` `plugins {}` table
//! (`"vendor/name" @ version`) resolves against a registry index that maps
//! `name -> versions -> { abi range, artifact URL }`. This module owns that
//! resolution: it selects a version whose declared plugin-ABI range
//! contains the host's ABI version — falling back to the newest compatible
//! build when the requested version does not (ARCHITECTURE.md §3.6) —
//! downloads the artifact on a cache miss, and verifies the downloaded
//! component really is what the index promised by instantiating it and
//! reading its `manifest` entry ([`crate::host::PluginHost::manifest_of_bytes`]).
//!
//! The index and its artifacts can be served either over HTTP(S) or from
//! the local filesystem (paths, `file://` URLs, or paths relative to an
//! index file on disk); the local mode exists so the registry client can
//! be tested and run without a network.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::host::{PluginHost, PluginManifest};

/// Upper bound on a single registry response (index document or plugin
/// artifact), matching the Maven resolver's artifact cap.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

/// A contiguous range of plugin-ABI versions a plugin version declares it
/// targets (ARCHITECTURE.md §3.7). The host picks a version whose range
/// contains its own ABI version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbiRange {
    /// Lowest plugin-ABI version the plugin works with.
    pub min: String,
    /// Highest plugin-ABI version the plugin works with.
    pub max: String,
}

impl AbiRange {
    /// True if `version` lies within `self`, inclusive.
    ///
    /// Version strings are compared as dot-separated numeric segments
    /// (`"0.1" == "0.1.0"`).
    ///
    /// # Examples
    ///
    /// ```
    /// use uliab::registry::AbiRange;
    ///
    /// let range = AbiRange {
    ///     min: "0.1".to_owned(),
    ///     max: "0.4".to_owned(),
    /// };
    /// assert!(range.contains("0.1.0"));
    /// assert!(range.contains("0.4"));
    /// assert!(!range.contains("0.5"));
    /// assert!(!range.contains("0.0.9"));
    /// ```
    #[must_use]
    pub fn contains(&self, version: &str) -> bool {
        compare_versions(&self.min, version) != Ordering::Greater
            && compare_versions(&self.max, version) != Ordering::Less
    }
}

/// One published version of a plugin: the ABI range it targets and where
/// its component artifact lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginVersionEntry {
    /// Plugin-ABI range this build declares support for.
    pub abi: AbiRange,
    /// Where the `plugin.wasm` component can be fetched from. An HTTP(S)
    /// URL, a `file://` URL, or a filesystem path (relative paths are
    /// resolved against the directory of the index file, when the index
    /// itself came from disk).
    pub artifact_url: String,
}

/// All published versions of one plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginIndexEntry {
    /// Versions keyed by version string.
    pub versions: BTreeMap<String, PluginVersionEntry>,
}

/// A `registry/index.json` document (ARCHITECTURE.md §3.6).
///
/// # Examples
///
/// ```
/// use uliab::registry::RegistryIndex;
///
/// let json = r#"{
///   "schema_version": 1,
///   "plugins": {
///     "ulite/hello": {
///       "versions": {
///         "0.1.0": {
///           "abi": { "min": "0.4", "max": "0.7" },
///           "artifact_url": "file:///tmp/hello_plugin.wasm"
///         }
///       }
///     }
///   }
/// }"#;
/// let index: RegistryIndex = serde_json::from_str(json).expect("valid index");
/// assert_eq!(index.schema_version, 1);
/// assert_eq!(index.plugins["ulite/hello"].versions["0.1.0"].abi.min, "0.4");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryIndex {
    /// Format version of the index document.
    pub schema_version: u32,
    /// Plugin catalog keyed by plugin name (`"ulite/hello"`).
    pub plugins: BTreeMap<String, PluginIndexEntry>,
}

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
    /// use uliab::registry::PluginSpec;
    /// use ulb_lang::eval::Value;
    ///
    /// let versioned = PluginSpec::from_value(&Value::Coordinate(
    ///     "ulite/hello:0.1.0".to_owned(),
    /// )).expect("valid coordinate");
    /// assert_eq!(versioned.name, "ulite/hello");
    /// assert_eq!(versioned.version.as_deref(), Some("0.1.0"));
    ///
    /// let unversioned = PluginSpec::from_value(&Value::Str("ulite/hello".to_owned()))
    ///     .expect("valid name");
    /// assert_eq!(unversioned.version, None);
    ///
    /// assert!(PluginSpec::from_value(&Value::Bool(true)).is_err());
    /// ```
    pub fn from_value(value: &ulb_lang::eval::Value) -> Result<Self, String> {
        use ulb_lang::eval::Value;
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
}

impl std::fmt::Display for PluginSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.version {
            Some(version) => write!(f, "{}@{}", self.name, version),
            None => write!(f, "{}", self.name),
        }
    }
}

/// Where a registry index comes from.
#[derive(Debug, Clone)]
pub enum RegistrySource {
    /// An index served over HTTP(S).
    Url(String),
    /// An `index.json` on the local filesystem. Relative `artifact_url`
    /// entries in it are resolved against this file's directory.
    File(PathBuf),
}

/// The outcome of resolving one plugin to a local artifact.
#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    /// Registry name the plugin resolved under.
    pub name: String,
    /// Version that was selected.
    pub version: String,
    /// ABI range the selected build declares.
    pub abi: AbiRange,
    /// Local path of the cached component.
    pub path: PathBuf,
    /// True when the artifact was already present in the cache and no
    /// fetch or verification was needed.
    pub from_cache: bool,
    /// A human-readable warning about a fallback (e.g. the requested
    /// version is ABI-incompatible with the host, so a compatible build
    /// was picked instead). `None` when resolution was straightforward.
    pub warning: Option<String>,
}

/// Errors raised while resolving a plugin.
#[derive(Debug)]
pub enum RegistryError {
    /// The index could not be fetched or parsed.
    Index(String),
    /// The plugin name is not in the index.
    UnknownPlugin(String),
    /// The requested version does not exist in the index.
    UnknownVersion {
        /// Plugin name.
        name: String,
        /// Requested version.
        version: String,
    },
    /// No version compatible with the host ABI exists.
    Incompatible {
        /// Plugin name.
        name: String,
        /// Host plugin-ABI version nothing matches.
        host_abi: String,
    },
    /// The artifact could not be fetched.
    Fetch {
        /// URL or path that failed.
        url: String,
        /// Underlying error.
        message: String,
    },
    /// The downloaded artifact failed verification.
    Verify(String),
    /// Cache read/write failure.
    Cache(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Index(message) => write!(f, "registry index unavailable: {message}"),
            Self::UnknownPlugin(name) => {
                write!(f, "plugin '{name}' is not published in the registry")
            }
            Self::UnknownVersion { name, version } => {
                write!(f, "plugin '{name}' has no published version '{version}'")
            }
            Self::Incompatible { name, host_abi } => {
                write!(
                    f,
                    "no published build of '{name}' supports plugin-ABI '{host_abi}'"
                )
            }
            Self::Fetch { url, message } => write!(f, "failed to fetch '{url}': {message}"),
            Self::Verify(message) => write!(f, "plugin artifact failed verification: {message}"),
            Self::Cache(message) => write!(f, "plugin cache error: {message}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// A verifier: given an artifact's bytes, returns its `manifest` entry.
///
/// The production verifier instantiates the component in an embedded
/// wasmtime host; tests substitute a stub so cache and selection logic can
/// be exercised without a real component.
type Verifier = Arc<dyn Fn(&[u8]) -> Result<PluginManifest, String> + Send + Sync>;

/// Resolves plugin coordinates against a registry index, downloading and
/// verifying artifacts into a local cache (ARCHITECTURE.md §3.6).
pub struct Registry {
    source: RegistrySource,
    cache_dir: PathBuf,
    host_abi: String,
    verifier: Verifier,
}

/// Metadata written next to a cached artifact so a later resolve can tell
/// which ABI range the cached build was verified against (and detect that
/// a host upgrade has made the cached artifact stale).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedResolution {
    name: String,
    /// Recorded for provenance in the on-disk metadata; resolution reads
    /// `name` and `abi` only.
    version: String,
    abi: AbiRange,
}

impl Registry {
    /// Creates a resolver against `source`, caching artifacts under
    /// `cache_dir` (or `$HOME/.cache/uliab/plugins` when `None`, per
    /// ARCHITECTURE.md §3.6).
    pub fn new(source: RegistrySource, cache_dir: Option<PathBuf>) -> Self {
        let verifier: Verifier = Arc::new(|bytes| {
            let host = PluginHost::new().map_err(|error| error.to_string())?;
            host.manifest_of_bytes(bytes)
                .map_err(|error| error.to_string())
        });
        Self {
            source,
            cache_dir: cache_dir.unwrap_or_else(default_cache_dir),
            host_abi: ulb_plugin_sdk::ABI_VERSION.to_owned(),
            verifier,
        }
    }

    /// Resolves `spec` to a local cached artifact.
    ///
    /// Selection picks the requested version when its declared ABI range
    /// contains the host ABI version, and otherwise falls back to the
    /// newest compatible build with a warning — the "last-known-compatible"
    /// behavior of ARCHITECTURE.md §3.6. With no requested version, the
    /// newest compatible build is chosen directly.
    ///
    /// On a cache hit the cached artifact is returned without refetching,
    /// unless its recorded ABI range no longer contains the host ABI
    /// version, in which case it is refetched (the tool was upgraded in
    /// between).
    ///
    /// On a cache miss the artifact is fetched and, before it is written
    /// to the cache, instantiated and cross-checked against the index
    /// entry: the `manifest` name and version must match the coordinate,
    /// and the manifest's ABI version must lie within the entry's declared
    /// range.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Index`] when the index cannot be fetched or
    /// parsed, [`RegistryError::UnknownPlugin`] / [`RegistryError::UnknownVersion`]
    /// when the coordinate has no match, [`RegistryError::Incompatible`]
    /// when no published build supports the host ABI, [`RegistryError::Fetch`]
    /// when the artifact cannot be downloaded, [`RegistryError::Verify`]
    /// when the downloaded component does not match its index entry, and
    /// [`RegistryError::Cache`] for cache read/write failures.
    pub fn resolve(&self, spec: &PluginSpec) -> Result<ResolvedPlugin, RegistryError> {
        let index = self.load_index()?;
        let entry = index
            .plugins
            .get(&spec.name)
            .ok_or_else(|| RegistryError::UnknownPlugin(spec.name.clone()))?;

        let (version, abi, warning) = select_version(entry, spec, &self.host_abi)?;

        let plugin_dir = self.cache_dir.join(&spec.name).join(&version);
        let wasm_path = plugin_dir.join("plugin.wasm");
        let meta_path = plugin_dir.join("abi.json");

        if let Some(meta) = self.read_cached(&wasm_path, &meta_path)
            && meta.name == spec.name
            && meta.abi.contains(&self.host_abi)
        {
            return Ok(ResolvedPlugin {
                name: spec.name.clone(),
                version,
                abi,
                path: wasm_path,
                from_cache: true,
                warning,
            });
        }

        let artifact_url = entry.versions[&version].artifact_url.clone();
        let bytes = self.fetch_artifact(&artifact_url)?;
        self.verify_artifact(&bytes, spec, &version, &abi)?;

        std::fs::create_dir_all(&plugin_dir)
            .map_err(|error| RegistryError::Cache(error.to_string()))?;
        std::fs::write(&wasm_path, &bytes)
            .map_err(|error| RegistryError::Cache(error.to_string()))?;
        let meta = CachedResolution {
            name: spec.name.clone(),
            version: version.clone(),
            abi: abi.clone(),
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|error| RegistryError::Cache(error.to_string()))?;
        std::fs::write(&meta_path, meta_json)
            .map_err(|error| RegistryError::Cache(error.to_string()))?;

        Ok(ResolvedPlugin {
            name: spec.name.clone(),
            version,
            abi,
            path: wasm_path,
            from_cache: false,
            warning,
        })
    }

    /// The cache root this resolver writes to.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn load_index(&self) -> Result<RegistryIndex, RegistryError> {
        let bytes = match &self.source {
            RegistrySource::File(path) => std::fs::read(path)
                .map_err(|error| RegistryError::Index(format!("{}: {error}", path.display())))?,
            RegistrySource::Url(url) => self.http_get(url).map_err(|error| {
                RegistryError::Index(format!("failed to fetch '{url}': {error}"))
            })?,
        };
        let index: RegistryIndex = serde_json::from_slice(&bytes)
            .map_err(|error| RegistryError::Index(error.to_string()))?;
        if index.schema_version != 1 {
            return Err(RegistryError::Index(format!(
                "unsupported index schema_version {} (this tool understands version 1); \
                 upgrade the tool or pin an older registry",
                index.schema_version
            )));
        }
        Ok(index)
    }

    fn read_cached(&self, wasm_path: &Path, meta_path: &Path) -> Option<CachedResolution> {
        if !wasm_path.exists() {
            return None;
        }
        // An unreadable or corrupt metadata file degrades to a refetch:
        // the artifact itself is re-verified against the index either way,
        // so treating metadata loss as a cache miss is safe.
        let bytes = std::fs::read(meta_path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn verify_artifact(
        &self,
        bytes: &[u8],
        spec: &PluginSpec,
        version: &str,
        abi: &AbiRange,
    ) -> Result<(), RegistryError> {
        let manifest = (self.verifier)(bytes)
            .map_err(|error| RegistryError::Verify(format!("{spec}@{version}: {error}")))?;
        if manifest.name != spec.name {
            return Err(RegistryError::Verify(format!(
                "artifact manifest name '{}' does not match requested '{}'",
                manifest.name, spec.name
            )));
        }
        if manifest.version != version {
            return Err(RegistryError::Verify(format!(
                "artifact manifest version '{}' does not match resolved '{}'",
                manifest.version, version
            )));
        }
        if !abi.contains(&manifest.abi_version) {
            return Err(RegistryError::Verify(format!(
                "artifact manifest abi-version '{}' lies outside the entry's declared range {}-{}",
                manifest.abi_version, abi.min, abi.max
            )));
        }
        Ok(())
    }

    fn fetch_artifact(&self, artifact_url: &str) -> Result<Vec<u8>, RegistryError> {
        if let Some(path) = artifact_url.strip_prefix("file://") {
            return read_file(Path::new(path), artifact_url);
        }
        if !artifact_url.contains("://") {
            // A filesystem path; relative entries resolve against the
            // index file's directory.
            let path = match &self.source {
                RegistrySource::File(index_path) => index_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(artifact_url),
                RegistrySource::Url(_) => PathBuf::from(artifact_url),
            };
            return read_file(&path, artifact_url);
        }
        self.http_get(artifact_url)
            .map_err(|error| RegistryError::Fetch {
                url: artifact_url.to_owned(),
                message: error,
            })
    }

    fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = ureq::get(url).call().map_err(|error| match error {
            ureq::Error::StatusCode(code) => format!("HTTP {code}"),
            other => other.to_string(),
        })?;
        // Capped like the Maven resolver's artifact downloads so a
        // misbehaving registry cannot exhaust memory with a huge body.
        let mut body = response.into_body().into_reader().take(MAX_RESPONSE_BYTES);
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)
            .map_err(|error| format!("reading response body: {error}"))?;
        Ok(bytes)
    }
}

fn read_file(path: &Path, label: &str) -> Result<Vec<u8>, RegistryError> {
    std::fs::read(path).map_err(|error| RegistryError::Fetch {
        url: label.to_owned(),
        message: error.to_string(),
    })
}

fn default_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".cache")
            .join("uliab")
            .join("plugins"),
        None => PathBuf::from(".cache").join("uliab").join("plugins"),
    }
}

/// Picks the version of `entry` to use for `spec` under host ABI
/// `host_abi`, implementing the fallback of ARCHITECTURE.md §3.6.
///
/// Returns the selected version, its ABI range, and — when a requested
/// version had to be replaced by a compatible build — a warning.
fn select_version(
    entry: &PluginIndexEntry,
    spec: &PluginSpec,
    host_abi: &str,
) -> Result<(String, AbiRange, Option<String>), RegistryError> {
    let requested = spec.version.as_deref();
    let mut warning = None;

    let selected = match requested {
        Some(version) => {
            let candidate =
                entry
                    .versions
                    .get(version)
                    .ok_or_else(|| RegistryError::UnknownVersion {
                        name: spec.name.clone(),
                        version: version.to_owned(),
                    })?;
            if candidate.abi.contains(host_abi) {
                Some((version, candidate))
            } else {
                match newest_compatible(entry, host_abi, Some(version)) {
                    Some(found) => {
                        warning = Some(format!(
                            "plugin '{}@{}' does not support plugin-ABI '{}'; \
                             falling back to last-known-compatible '{}'",
                            spec.name, version, host_abi, found.0
                        ));
                        Some(found)
                    }
                    None => {
                        return Err(RegistryError::Incompatible {
                            name: spec.name.clone(),
                            host_abi: host_abi.to_owned(),
                        });
                    }
                }
            }
        }
        None => newest_compatible(entry, host_abi, None),
    };

    match selected {
        Some((version, candidate)) => Ok((version.to_owned(), candidate.abi.clone(), warning)),
        None => Err(RegistryError::Incompatible {
            name: spec.name.clone(),
            host_abi: host_abi.to_owned(),
        }),
    }
}

/// The newest version whose ABI range contains `host_abi`, skipping
/// `skip` when given.
fn newest_compatible<'a>(
    entry: &'a PluginIndexEntry,
    host_abi: &str,
    skip: Option<&str>,
) -> Option<(&'a str, &'a PluginVersionEntry)> {
    entry
        .versions
        .iter()
        .filter(|(version, candidate)| {
            *version != skip.unwrap_or_default() && candidate.abi.contains(host_abi)
        })
        .max_by(|(a, _), (b, _)| compare_versions(a, b))
        .map(|(version, candidate)| (version.as_str(), candidate))
}

/// Compares two version strings as dot-separated numeric segments;
/// missing trailing segments count as zero (`"0.1" == "0.1.0"`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut a_parts = a.split('.').map(parse_segment).peekable();
    let mut b_parts = b.split('.').map(parse_segment).peekable();
    loop {
        let a_segment = a_parts.next().unwrap_or(0);
        let b_segment = b_parts.next().unwrap_or(0);
        match a_segment.cmp(&b_segment) {
            Ordering::Equal => {
                if a_parts.peek().is_none() && b_parts.peek().is_none() {
                    return Ordering::Equal;
                }
            }
            other => return other,
        }
    }
}

fn parse_segment(segment: &str) -> u64 {
    segment.parse().unwrap_or(0)
}

fn value_kind(value: &ulb_lang::eval::Value) -> &'static str {
    use ulb_lang::eval::Value;
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
    use ulb_lang::eval::Value;

    /// A registry source whose index and artifacts live in one temp dir.
    fn fixture_source(dir: &Path) -> RegistrySource {
        RegistrySource::File(dir.join("index.json"))
    }

    fn write_index(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).expect("create fixture dir");
        std::fs::write(dir.join("index.json"), json).expect("write index");
    }

    fn fake_verifier() -> Verifier {
        fake_verifier_named("0.1.0")
    }

    fn fake_verifier_named(version: &str) -> Verifier {
        let version = version.to_owned();
        Arc::new(move |_| {
            Ok(PluginManifest {
                name: "ulite/hello".to_owned(),
                version: version.clone(),
                abi_version: ulb_plugin_sdk::ABI_VERSION.to_owned(),
                tools: vec![],
                dependencies: Vec::new(),
            })
        })
    }

    fn registry_with(source: RegistrySource, cache_dir: PathBuf, verifier: Verifier) -> Registry {
        Registry {
            source,
            cache_dir,
            host_abi: ulb_plugin_sdk::ABI_VERSION.to_owned(),
            verifier,
        }
    }

    fn sample_index() -> &'static str {
        r#"{
          "schema_version": 1,
          "plugins": {
            "ulite/hello": {
              "versions": {
                "0.1.0": {
                  "abi": { "min": "0.4", "max": "0.7" },
                  "artifact_url": "artifacts/hello_plugin.wasm"
                },
                "0.2.0": {
                  "abi": { "min": "0.4", "max": "0.7" },
                  "artifact_url": "artifacts/hello_plugin.wasm"
                }
              }
            }
          }
        }"#
    }

    #[test]
    fn parses_index_document() {
        let index: RegistryIndex = serde_json::from_str(sample_index()).expect("valid");
        assert_eq!(index.schema_version, 1);
        let hello = &index.plugins["ulite/hello"];
        assert_eq!(hello.versions.len(), 2);
        assert_eq!(hello.versions["0.1.0"].abi.min, "0.4");
    }

    #[test]
    fn rejects_malformed_index() {
        let error = serde_json::from_str::<RegistryIndex>(r#"{ "plugins": 3 }"#).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn spec_from_coordinate_and_string() {
        let spec = PluginSpec::from_value(&Value::Coordinate("ulite/hello:1.2.3".to_owned()))
            .expect("coordinate");
        assert_eq!(spec.name, "ulite/hello");
        assert_eq!(spec.version.as_deref(), Some("1.2.3"));

        let spec = PluginSpec::from_value(&Value::Str("ulite/hello".to_owned())).expect("string");
        assert_eq!(spec.version, None);

        assert!(PluginSpec::from_value(&Value::Bool(true)).is_err());
        assert!(PluginSpec::from_value(&Value::Invalid("boom".to_owned())).is_err());
    }

    #[test]
    fn abi_range_containment() {
        let range = AbiRange {
            min: "0.1".to_owned(),
            max: "0.2".to_owned(),
        };
        assert!(range.contains("0.1"));
        assert!(range.contains("0.1.0"));
        assert!(range.contains("0.2.0"));
        assert!(!range.contains("0.0.9"));
        assert!(!range.contains("0.3"));
    }

    #[test]
    fn compares_numeric_versions() {
        assert_eq!(compare_versions("0.1", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.9.0", "0.10.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.2.0", "0.2"), Ordering::Equal);
    }

    #[test]
    fn resolves_newest_compatible_without_pinning() {
        let index: RegistryIndex = serde_json::from_str(sample_index()).expect("valid");
        let entry = &index.plugins["ulite/hello"];
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: None,
        };
        let (version, abi, warning) =
            select_version(entry, &spec, ulb_plugin_sdk::ABI_VERSION).expect("resolvable");
        assert_eq!(version, "0.2.0");
        assert_eq!(abi.min, "0.4");
        assert!(warning.is_none());
    }

    #[test]
    fn falls_back_when_pinned_version_is_incompatible() {
        let json = r#"{
          "schema_version": 1,
          "plugins": {
            "ulite/hello": {
              "versions": {
                "0.1.0": { "abi": { "min": "0.1", "max": "0.1" }, "artifact_url": "a" },
                "0.3.0": { "abi": { "min": "0.2", "max": "0.2" }, "artifact_url": "b" }
              }
            }
          }
        }"#;
        let index: RegistryIndex = serde_json::from_str(json).expect("valid");
        let entry = &index.plugins["ulite/hello"];
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: Some("0.3.0".to_owned()),
        };
        let (version, _abi, warning) =
            select_version(entry, &spec, "0.1").expect("fallback available");
        assert_eq!(version, "0.1.0");
        assert!(warning.unwrap().contains("0.3.0"));
    }

    #[test]
    fn errors_when_nothing_compatible() {
        let json = r#"{
          "schema_version": 1,
          "plugins": {
            "ulite/hello": {
              "versions": {
                "0.1.0": { "abi": { "min": "0.1", "max": "0.1" }, "artifact_url": "a" }
              }
            }
          }
        }"#;
        let index: RegistryIndex = serde_json::from_str(json).expect("valid");
        let entry = &index.plugins["ulite/hello"];
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: Some("0.1.0".to_owned()),
        };
        let error = select_version(entry, &spec, "0.2").expect_err("incompatible");
        assert!(matches!(error, RegistryError::Incompatible { .. }));
    }

    #[test]
    fn unknown_plugin_and_version_errors() {
        let index: RegistryIndex = serde_json::from_str(sample_index()).expect("valid");
        let entry = &index.plugins["ulite/hello"];
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: Some("9.9.9".to_owned()),
        };
        let error = select_version(entry, &spec, "0.1").expect_err("no such version");
        assert!(matches!(error, RegistryError::UnknownVersion { .. }));
    }

    #[test]
    fn downloads_and_verifies_on_cache_miss() {
        let dir = temp_dir("miss");
        write_index(&dir, sample_index());
        std::fs::create_dir_all(dir.join("artifacts")).expect("artifacts dir");
        std::fs::write(dir.join("artifacts/hello_plugin.wasm"), b"wasm-bytes").expect("artifact");

        let cache = temp_dir("miss-cache");
        let registry = registry_with(
            fixture_source(&dir),
            cache.clone(),
            fake_verifier_named("0.2.0"),
        );
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: None,
        };
        let resolved = registry.resolve(&spec).expect("resolves");
        assert!(!resolved.from_cache);
        assert_eq!(resolved.version, "0.2.0");
        assert_eq!(resolved.path, cache.join("ulite/hello/0.2.0/plugin.wasm"));
        assert!(resolved.path.exists());
        assert_eq!(
            std::fs::read(&resolved.path).expect("wasm written"),
            b"wasm-bytes"
        );
    }

    #[test]
    fn cache_hit_skips_fetch() {
        let dir = temp_dir("hit");
        write_index(&dir, sample_index());
        let cache = temp_dir("hit-cache");
        let plugin_dir = cache.join("ulite/hello/0.2.0");
        std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
        std::fs::write(plugin_dir.join("plugin.wasm"), b"cached").expect("wasm");
        let meta = CachedResolution {
            name: "ulite/hello".to_owned(),
            version: "0.2.0".to_owned(),
            abi: AbiRange {
                min: "0.4".to_owned(),
                max: "0.7".to_owned(),
            },
        };
        std::fs::write(
            plugin_dir.join("abi.json"),
            serde_json::to_string_pretty(&meta).expect("meta"),
        )
        .expect("write meta");

        // The artifact URL is intentionally bogus: a hit must never fetch.
        let json = sample_index().replace(
            r#""artifact_url": "artifacts/hello_plugin.wasm""#,
            r#""artifact_url": "http://127.0.0.1:1/nope""#,
        );
        write_index(&dir, &json);

        let registry = registry_with(fixture_source(&dir), cache.clone(), fake_verifier());
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: None,
        };
        let resolved = registry.resolve(&spec).expect("cache hit");
        assert!(resolved.from_cache);
        assert_eq!(
            std::fs::read(&resolved.path).expect("cached wasm"),
            b"cached"
        );
    }

    #[test]
    fn stale_cache_refetches() {
        // A cache entry records the ABI range it was verified under; when a
        // later resolve runs under a host ABI outside that range (the tool
        // was upgraded), the cached artifact must be refetched rather than
        // trusted.
        let json = r#"{
          "schema_version": 1,
          "plugins": {
            "ulite/hello": {
              "versions": {
                "0.1.0": {
                  "abi": { "min": "0.1", "max": "0.1" },
                  "artifact_url": "artifacts/hello_plugin.wasm"
                },
                "0.2.0": {
                  "abi": { "min": "0.4", "max": "0.7" },
                  "artifact_url": "artifacts/hello_plugin.wasm"
                }
              }
            }
          }
        }"#;
        let dir = temp_dir("stale");
        write_index(&dir, json);
        std::fs::create_dir_all(dir.join("artifacts")).expect("artifacts dir");
        std::fs::write(dir.join("artifacts/hello_plugin.wasm"), b"fresh").expect("artifact");

        let cache = temp_dir("stale-cache");
        let plugin_dir = cache.join("ulite/hello/0.2.0");
        std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
        std::fs::write(plugin_dir.join("plugin.wasm"), b"old").expect("wasm");
        let meta = CachedResolution {
            name: "ulite/hello".to_owned(),
            version: "0.2.0".to_owned(),
            abi: AbiRange {
                min: "0.2".to_owned(),
                max: "0.2".to_owned(),
            },
        };
        std::fs::write(
            plugin_dir.join("abi.json"),
            serde_json::to_string_pretty(&meta).expect("meta"),
        )
        .expect("write meta");

        let registry = Registry {
            source: fixture_source(&dir),
            cache_dir: cache.clone(),
            host_abi: ulb_plugin_sdk::ABI_VERSION.to_owned(),
            verifier: fake_verifier_named("0.2.0"),
        };
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: None,
        };
        let resolved = registry.resolve(&spec).expect("refetched");
        assert!(!resolved.from_cache);
        assert_eq!(resolved.version, "0.2.0");
        assert_eq!(
            std::fs::read(&resolved.path).expect("refetched wasm"),
            b"fresh"
        );
    }

    #[test]
    fn verification_rejects_mismatched_artifact() {
        let dir = temp_dir("verify");
        write_index(&dir, sample_index());
        std::fs::create_dir_all(dir.join("artifacts")).expect("artifacts dir");
        std::fs::write(dir.join("artifacts/hello_plugin.wasm"), b"wasm-bytes").expect("artifact");

        let cache = temp_dir("verify-cache");
        let lying_verifier: Verifier = Arc::new(|_| {
            Ok(PluginManifest {
                name: "ulite/other".to_owned(),
                version: "0.2.0".to_owned(),
                abi_version: ulb_plugin_sdk::ABI_VERSION.to_owned(),
                tools: vec![],
                dependencies: Vec::new(),
            })
        });
        let registry = registry_with(fixture_source(&dir), cache, lying_verifier);
        let spec = PluginSpec {
            name: "ulite/hello".to_owned(),
            version: None,
        };
        let error = registry.resolve(&spec).expect_err("mismatch");
        assert!(matches!(error, RegistryError::Verify(_)));
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "uliab-registry-test-{}-{}",
            std::process::id(),
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
}
