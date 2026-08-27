//! Plugin config schema extraction from `.wasm` custom sections
//! (ARCHITECTURE.md §3.8).
//!
//! A plugin built with `#[derive(UlbConfig)]` embeds its config schema as
//! a JSON blob inside a wasm custom section named
//! [`SCHEMA_CUSTOM_SECTION`]. This crate reads that section from the raw
//! `.wasm` bytes — *without* instantiating or running the plugin — so the
//! host and CLI can inspect a plugin's declared DSL surface ahead of any
//! build.
//!
//! Plugins built before the schema feature carry no schema section;
//! callers treat [`extract_schema`] returning `None` as "schema
//! unavailable" (silently degraded, not an error).
//!
//! This crate is consumed by both the `uliab` host and the `ulb-lsp`
//! editor integration. The host uses [`validate_plugin_config`] to
//! reject typos in plugin-owned blocks during builds; the LSP uses the
//! same types to drive completions and hover documentation.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// The wasm custom section name where a `#[derive(UlbConfig)]` plugin
/// embeds its config schema.
pub const SCHEMA_CUSTOM_SECTION: &str = "ulb-config-schema";

/// A single field in a plugin's config schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemaField {
    /// The JSON key name (matches the Rust field name or its
    /// `#[ulb(rename = "...")]` override).
    pub name: String,
    /// Primitive type: `"string"`, `"integer"`, `"boolean"`, `"object"`,
    /// `"array"`, or `"enum"`.
    pub type_name: String,
    /// Human-readable description from the field's `///` doc comment or
    /// `#[ulb(description = "...")]` override.
    pub description: String,
    /// `true` when the Rust field is not wrapped in `Option<T>`.
    pub required: bool,
    /// For `"object"` fields: the nested field list. Empty when the inner
    /// type does not itself derive `UlbConfig`.
    #[serde(default)]
    pub properties: Vec<SchemaField>,
    /// For `"array"` fields: the element type name. Absent for non-array
    /// types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<String>,
    /// For `"enum"` fields: the list of variant names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
}

/// The top-level schema for a single plugin's config block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginSchema {
    /// The plugin's registry name, e.g. `"ulite/android"`.
    pub name: String,
    /// The schema's top-level fields — one per key the plugin reads from
    /// its module config block.
    pub properties: Vec<SchemaField>,
}

/// Extracts a plugin's config schema from the `ulb-config-schema` custom
/// section in raw `.wasm` bytes.
///
/// Returns `None` when the section is absent (plugin built before the
/// schema feature) or when the section content is not valid JSON matching
/// [`PluginSchema`].
///
/// # Examples
///
/// See `tests/schema_roundtrip.rs` in the `uliab` crate for a full
/// extraction example that builds a real plugin and reads its embedded
/// schema. The unit tests below cover the extraction edge cases.
pub fn extract_schema(wasm_bytes: &[u8]) -> Option<PluginSchema> {
    // Try module parser first (works for core wasm modules).
    if let Some(schema) = extract_schema_inner(wasm_bytes, false) {
        return Some(schema);
    }
    // Fall back to component parser (works for wasm32-wasip2 components
    // that wrap the core module).
    extract_schema_inner(wasm_bytes, true)
}

fn extract_schema_inner(wasm_bytes: &[u8], component_model: bool) -> Option<PluginSchema> {
    let mut parser = wasmparser::Parser::new(0);
    if component_model {
        let mut features = parser.features();
        features |= wasmparser::WasmFeatures::COMPONENT_MODEL;
        parser.set_features(features);
    }
    for result in parser.parse_all(wasm_bytes) {
        let payload = match result {
            Ok(payload) => payload,
            // A malformed trailer before the custom section is treated as
            // "section absent" so extraction degrades to `None` instead of
            // failing the whole parse; the caller reports that as schema
            // unavailable (ARCHITECTURE.md §3.8).
            Err(_) => continue,
        };

        if let wasmparser::Payload::CustomSection(custom) = payload
            && custom.name() == SCHEMA_CUSTOM_SECTION
        {
            let mut data = custom.data();
            // The `embed_schema!` macro null-terminates the bytes for
            // defense-in-depth; strip it before JSON parsing.
            if data.last() == Some(&0) {
                data = &data[..data.len() - 1];
            }
            return serde_json::from_slice(data).ok();
        }
    }

    None
}

/// Returns the raw bytes of the `ulb-config-schema` custom section, if
/// present. Unlike [`extract_schema`], this does not deserialize — useful
/// for forwarding the schema to the LSP or for debugging.
pub fn raw_schema_section(wasm_bytes: &[u8]) -> Option<Vec<u8>> {
    if let Some(raw) = raw_schema_section_inner(wasm_bytes, false) {
        return Some(raw);
    }
    raw_schema_section_inner(wasm_bytes, true)
}

fn raw_schema_section_inner(wasm_bytes: &[u8], component_model: bool) -> Option<Vec<u8>> {
    let mut parser = wasmparser::Parser::new(0);
    if component_model {
        let mut features = parser.features();
        features |= wasmparser::WasmFeatures::COMPONENT_MODEL;
        parser.set_features(features);
    }
    for result in parser.parse_all(wasm_bytes) {
        let payload = match result {
            Ok(payload) => payload,
            // Same degraded-mode behavior as `extract_schema_inner`: a
            // malformed section is reported as absent, never as an error.
            Err(_) => continue,
        };

        if let wasmparser::Payload::CustomSection(custom) = payload
            && custom.name() == SCHEMA_CUSTOM_SECTION
        {
            return Some(custom.data().to_vec());
        }
    }

    None
}

/// Validates a plugin's config JSON against its declared schema.
///
/// Only validates keys **inside** objects whose schema declares nested
/// properties (e.g. the `android {}` block). Top-level unknown keys are
/// allowed because the driver sends the entire module model to every
/// plugin, and other plugins' keys coexist at the top level.
///
/// Returns `Ok(())` when every checked key matches a declared field.
/// Returns `Err(errors)` with one message per unknown key, each
/// including a "did you mean?" suggestion when the edit distance is
/// small enough.
///
/// When the plugin has no schema ([`None`] from [`extract_schema`]),
/// callers should skip validation — degraded mode.
///
/// # Errors
///
/// Returns `Err` containing validation messages when a plugin-owned
/// block contains keys not declared in the schema.
///
/// # Examples
///
/// ```rust
/// use ulb_schema::{PluginSchema, SchemaField, validate_plugin_config};
///
/// let schema = PluginSchema {
///     name: "ulite/android".into(),
///     properties: vec![SchemaField {
///         name: "android".into(),
///         type_name: "object".into(),
///         description: "Android build configuration.".into(),
///         required: true,
///         properties: vec![SchemaField {
///             name: "compileSdk".into(),
///             type_name: "integer".into(),
///             description: "Android compile SDK version.".into(),
///             required: true,
///             properties: vec![],
///             items: None,
///             variants: vec![],
///         }],
///         items: None,
///         variants: vec![],
///     }],
/// };
///
/// let valid = serde_json::json!({ "android": { "compileSdk": 36 } });
/// assert!(validate_plugin_config(&schema, &valid).is_ok());
///
/// let invalid = serde_json::json!({ "android": { "compileSdk": 36, "complieSdk": 99 } });
/// let errors = validate_plugin_config(&schema, &invalid).unwrap_err();
/// assert!(errors[0].contains("did you mean"));
/// ```
pub fn validate_plugin_config(
    schema: &PluginSchema,
    config: &serde_json::Value,
) -> Result<(), Vec<String>> {
    let Some(object) = config.as_object() else {
        return Ok(());
    };
    validate_nested_objects(&schema.properties, object)
}

/// For each schema field that declares an `"object"` type with nested
/// properties, validates the keys inside that object in the config.
fn validate_nested_objects(
    fields: &[SchemaField],
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    for field in fields {
        if field.type_name != "object" || field.properties.is_empty() {
            continue;
        }
        let Some(nested_value) = object.get(&field.name) else {
            continue;
        };
        let Some(nested_object) = nested_value.as_object() else {
            continue;
        };
        if let Err(nested_errors) = validate_object(&field.properties, nested_object, &field.name) {
            errors.extend(nested_errors);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validates all keys in `object` against `fields`, rejecting unknown
/// keys with "did you mean?" suggestions.  Used for plugin-owned
/// blocks where every key should match a declared schema field.
fn validate_object(
    fields: &[SchemaField],
    object: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let declared: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();

    for key in object.keys() {
        if declared.contains(&key.as_str()) {
            continue;
        }

        let suggestion = suggest_key(key, &declared);
        let msg = match suggestion {
            Some(hint) => format!("unknown key '{prefix}.{key}' — did you mean '{hint}'?"),
            None => format!("unknown key '{prefix}.{key}'"),
        };
        errors.push(msg);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Returns the closest matching candidate from `candidates` using
/// Levenshtein distance, or `None` when the best match is farther
/// than 3 edits away.
pub fn suggest_key(input: &str, candidates: &[&str]) -> Option<String> {
    let mut best_dist = usize::MAX;
    let mut best: Option<String> = None;
    for candidate in candidates {
        let dist = levenshtein(input, candidate);
        if dist < best_dist {
            best_dist = dist;
            best = Some(candidate.to_string());
        }
    }
    if best_dist <= 3 { best } else { None }
}

/// Levenshtein edit distance between two strings.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev = (0..=b_len).collect::<Vec<usize>>();
    let mut curr = vec![0usize; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = usize::from(a_bytes[i - 1] != b_bytes[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes `value` as unsigned LEB128 into `buf`.
    fn encode_leb128(buf: &mut Vec<u8>, mut value: usize) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    /// Builds a minimal wasm module with a single custom section containing
    /// `payload` under the name `section_name`.
    fn wasm_with_custom_section(section_name: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic: \0asm
            0x01, 0x00, 0x00, 0x00, // version: 1
        ];
        // Section id 0 = custom section
        wasm.push(0x00);
        // Section body = name_len(name) + payload
        let section_body_len = 1 + section_name.len() + payload.len();
        encode_leb128(&mut wasm, section_body_len);
        // Name length (LEB128, always ≤127 for our short names)
        encode_leb128(&mut wasm, section_name.len());
        wasm.extend_from_slice(section_name);
        wasm.extend_from_slice(payload);
        wasm
    }

    #[test]
    fn extract_schema_returns_none_for_empty_bytes() {
        assert!(extract_schema(&[]).is_none());
    }

    #[test]
    fn extract_schema_returns_none_when_section_absent() {
        let wasm = wasm_with_custom_section(b"other-section", b"data");
        assert!(extract_schema(&wasm).is_none());
    }

    #[test]
    fn extract_schema_returns_none_for_invalid_json_in_section() {
        let wasm = wasm_with_custom_section(SCHEMA_CUSTOM_SECTION.as_bytes(), b"not valid json");
        assert!(extract_schema(&wasm).is_none());
    }

    #[test]
    fn extract_schema_round_trips() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "compile_sdk".to_owned(),
                type_name: "integer".to_owned(),
                description: "The compile SDK version.".to_owned(),
                required: true,
                properties: vec![],
                items: None,
                variants: vec![],
            }],
        };

        let json = serde_json::to_vec(&schema).unwrap();
        let wasm = wasm_with_custom_section(SCHEMA_CUSTOM_SECTION.as_bytes(), &json);

        let extracted = extract_schema(&wasm).expect("schema present");
        assert_eq!(extracted, schema);
    }

    #[test]
    fn extract_schema_component_model_fallback() {
        // `embed_schema!` null-terminates the embedded bytes. Verify the
        // extraction strips a trailing NUL and still round-trips even
        // when the payload is wrapped the way a real plugin embeds it.
        let schema = PluginSchema {
            name: "ulite/component".to_owned(),
            properties: vec![],
        };
        let mut json = serde_json::to_vec(&schema).unwrap();
        json.push(0);
        let wasm = wasm_with_custom_section(SCHEMA_CUSTOM_SECTION.as_bytes(), &json);

        let extracted = extract_schema(&wasm).expect("schema present");
        assert_eq!(extracted, schema);
    }

    #[test]
    fn raw_schema_section_returns_raw_bytes() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![],
        };
        let json = serde_json::to_vec(&schema).unwrap();
        let wasm = wasm_with_custom_section(SCHEMA_CUSTOM_SECTION.as_bytes(), &json);

        let raw = raw_schema_section(&wasm).expect("section present");
        assert_eq!(raw, json);
    }

    #[test]
    fn raw_schema_section_returns_none_when_absent() {
        let wasm = wasm_with_custom_section(b"other", b"data");
        assert!(raw_schema_section(&wasm).is_none());
    }

    // ── Levenshtein tests ─────────────────────────────────────────────

    #[test]
    fn levenshtein_identical_strings() {
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    #[test]
    fn levenshtein_single_insert() {
        assert_eq!(levenshtein("abc", "abcd"), 1);
    }

    #[test]
    fn levenshtein_single_delete() {
        assert_eq!(levenshtein("abcd", "abc"), 1);
    }

    #[test]
    fn levenshtein_single_substitution() {
        assert_eq!(levenshtein("abc", "axc"), 1);
    }

    #[test]
    fn levenshtein_completely_different() {
        assert_eq!(levenshtein("abc", "xyz"), 3);
    }

    #[test]
    fn levenshtein_empty_strings() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    // ── suggest_key tests ─────────────────────────────────────────────

    #[test]
    fn suggest_key_returns_closest_match() {
        let candidates = ["compose", "sources", "manifest"];
        assert_eq!(
            suggest_key("compos", &candidates),
            Some("compose".to_owned())
        );
    }

    #[test]
    fn suggest_key_returns_none_for_distant() {
        let candidates = ["compose", "sources", "manifest"];
        assert_eq!(suggest_key("xyzzy", &candidates), None);
    }

    #[test]
    fn suggest_key_returns_none_for_empty_candidates() {
        assert_eq!(suggest_key("anything", &[]), None);
    }

    #[test]
    fn suggest_key_prefers_exact_prefix_match() {
        let candidates = ["buildTypes", "buildConfigField"];
        // "buildType" is 1 away from "buildTypes"
        let suggestion = suggest_key("buildType", &candidates).unwrap();
        assert_eq!(suggestion, "buildTypes");
    }

    // ── validate_plugin_config tests ──────────────────────────────────

    fn make_schema(fields: Vec<(&str, &str, bool)>) -> PluginSchema {
        PluginSchema {
            name: "ulite/test".to_owned(),
            properties: fields
                .into_iter()
                .map(|(name, type_name, required)| SchemaField {
                    name: name.to_owned(),
                    type_name: type_name.to_owned(),
                    description: String::new(),
                    required,
                    properties: vec![],
                    items: None,
                    variants: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn validate_accepts_valid_config() {
        let schema = make_schema(vec![
            ("projectDir", "string", true),
            ("android", "object", true),
        ]);
        let config = serde_json::json!({
            "projectDir": "/proj",
            "android": { "compileSdk": 36 }
        });
        assert!(validate_plugin_config(&schema, &config).is_ok());
    }

    #[test]
    fn validate_top_level_unknown_keys_are_allowed() {
        // The driver sends the full module model to every plugin.
        // Other plugins' keys coexist at the top level and must not
        // trigger validation errors.
        let schema = make_schema(vec![
            ("projectDir", "string", true),
            ("android", "object", true),
        ]);
        let config = serde_json::json!({
            "projectDir": "/proj",
            "jvm": { "sources": ["Main.java"] },
            "android": {}
        });
        assert!(validate_plugin_config(&schema, &config).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_key_inside_nested_object() {
        let nested_field = SchemaField {
            name: "compileSdk".to_owned(),
            type_name: "integer".to_owned(),
            description: String::new(),
            required: true,
            properties: vec![],
            items: None,
            variants: vec![],
        };
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "android".to_owned(),
                type_name: "object".to_owned(),
                description: String::new(),
                required: true,
                properties: vec![nested_field],
                items: None,
                variants: vec![],
            }],
        };
        let config = serde_json::json!({
            "android": { "compileSdk": 36, "complieSdk": 99 }
        });
        let errors = validate_plugin_config(&schema, &config).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("complieSdk"));
        assert!(errors[0].contains("did you mean"));
    }

    #[test]
    fn validate_nested_object_non_object_value_is_skipped() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "android".to_owned(),
                type_name: "object".to_owned(),
                description: String::new(),
                required: true,
                properties: vec![SchemaField {
                    name: "compileSdk".to_owned(),
                    type_name: "integer".to_owned(),
                    description: String::new(),
                    required: true,
                    properties: vec![],
                    items: None,
                    variants: vec![],
                }],
                items: None,
                variants: vec![],
            }],
        };
        // "android" is a string, not an object — validation skips it.
        let config = serde_json::json!({ "android": "not_an_object" });
        assert!(validate_plugin_config(&schema, &config).is_ok());
    }

    #[test]
    fn suggest_key_rejects_distant_matches() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "android".to_owned(),
                type_name: "object".to_owned(),
                description: String::new(),
                required: true,
                properties: vec![SchemaField {
                    name: "compileSdk".to_owned(),
                    type_name: "integer".to_owned(),
                    description: String::new(),
                    required: true,
                    properties: vec![],
                    items: None,
                    variants: vec![],
                }],
                items: None,
                variants: vec![],
            }],
        };
        let config = serde_json::json!({ "android": { "totallyUnrelated": 1 } });
        let errors = validate_plugin_config(&schema, &config).unwrap_err();
        assert_eq!(errors.len(), 1);
        // "totallyUnrelated" is too far from "compileSdk" — no suggestion.
        assert!(!errors[0].contains("did you mean"));
    }

    #[test]
    fn validate_no_error_for_non_object_config() {
        let schema = make_schema(vec![("x", "string", true)]);
        let config = serde_json::json!("just a string");
        assert!(validate_plugin_config(&schema, &config).is_ok());
    }

    #[test]
    fn validate_skips_unknown_when_no_properties_declared() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "kmp".to_owned(),
                type_name: "object".to_owned(),
                description: String::new(),
                required: true,
                properties: vec![], // empty = dynamic map, no recursion
                items: None,
                variants: vec![],
            }],
        };
        let config = serde_json::json!({
            "kmp": { "commonMain": {}, "jvm": {} }
        });
        assert!(validate_plugin_config(&schema, &config).is_ok());
    }

    #[test]
    fn validate_multiple_unknown_keys_in_nested_object() {
        let schema = PluginSchema {
            name: "ulite/test".to_owned(),
            properties: vec![SchemaField {
                name: "block".to_owned(),
                type_name: "object".to_owned(),
                description: String::new(),
                required: true,
                properties: vec![SchemaField {
                    name: "sources".to_owned(),
                    type_name: "array".to_owned(),
                    description: String::new(),
                    required: true,
                    properties: vec![],
                    items: None,
                    variants: vec![],
                }],
                items: None,
                variants: vec![],
            }],
        };
        let config = serde_json::json!({
            "block": { "sources": [], "typo1": 1, "typo2": 2 }
        });
        let errors = validate_plugin_config(&schema, &config).unwrap_err();
        assert_eq!(errors.len(), 2);
    }
}
