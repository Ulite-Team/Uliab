//! Plugin config schema extraction from `.wasm` custom sections
//! (ARCHITECTURE.md §3.8).
//!
//! A plugin built with `#[derive(UlbConfig)]` embeds its config schema as
//! a JSON blob inside a wasm custom section named
//! [`SCHEMA_CUSTOM_SECTION`]. This module reads that section from the raw
//! `.wasm` bytes — *without* instantiating or running the plugin — so the
//! host and CLI can inspect a plugin's declared DSL surface ahead of any
//! build.
//!
//! Plugins built before the schema feature carry no schema section;
//! callers treat [`extract_schema`] returning `None` as "schema
//! unavailable" (silently degraded, not an error).

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
/// ```rust,ignore
/// let wasm = std::fs::read("plugin.wasm")?;
/// match extract_schema(&wasm) {
///     Some(schema) => println!("{} has {} fields", schema.name, schema.properties.len()),
///     None => println!("no schema section — degraded mode"),
/// }
/// ```
pub fn extract_schema(wasm_bytes: &[u8]) -> Option<PluginSchema> {
    for result in wasmparser::Parser::new(0).parse_all(wasm_bytes) {
        let payload = match result {
            Ok(payload) => payload,
            Err(_) => return None,
        };

        if let wasmparser::Payload::CustomSection(custom) = payload
            && custom.name() == SCHEMA_CUSTOM_SECTION
        {
            return serde_json::from_slice(custom.data()).ok();
        }
    }

    None
}

/// Returns the raw bytes of the `ulb-config-schema` custom section, if
/// present. Unlike [`extract_schema`], this does not deserialize — useful
/// for forwarding the schema to the LSP or for debugging.
pub fn raw_schema_section(wasm_bytes: &[u8]) -> Option<Vec<u8>> {
    for result in wasmparser::Parser::new(0).parse_all(wasm_bytes) {
        let payload = match result {
            Ok(payload) => payload,
            Err(_) => return None,
        };

        if let wasmparser::Payload::CustomSection(custom) = payload
            && custom.name() == SCHEMA_CUSTOM_SECTION
        {
            return Some(custom.data().to_vec());
        }
    }

    None
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
}
