//! Reader for the `ulb:config-schema` custom section (ARCHITECTURE.md §3.8).
//!
//! A plugin built with `#[derive(UlbConfig)]` embeds its declared key and
//! feature catalog as a custom section in the compiled artifact. This
//! module reads that section out of the raw bytes — no wasmtime
//! instantiation, no execution — so the host's `describe` tooling and the
//! LSP can present a plugin's surface from the artifact alone.
//!
//! The reader handles both plain WebAssembly core modules and components
//! (the wasip2 output shape), walking one level of encapsulation to find
//! the inner core module's custom sections. Absence of the section is not
//! an error: a plugin built before schemas existed simply yields
//! `Ok(None)` and consumers degrade gracefully.

use std::collections::BTreeMap;

/// The custom-section name a derived config catalog is embedded under.
pub const SCHEMA_SECTION_NAME: &str = "ulb:config-schema";

/// The kind of value an owned key accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// Integer primitive (`i64`/`u32`-shaped DSL numbers).
    Int,
    /// Text value.
    String,
    /// Boolean toggle; feature toggles are always booleans.
    Bool,
    /// List of strings.
    List,
    /// Nested configuration block.
    Block,
}

impl KeyKind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "int" => Some(Self::Int),
            "string" => Some(Self::String),
            "bool" => Some(Self::Bool),
            "list" => Some(Self::List),
            "block" => Some(Self::Block),
            _ => None,
        }
    }

    /// The catalog spelling of this kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::String => "string",
            Self::Bool => "bool",
            Self::List => "list",
            Self::Block => "block",
        }
    }
}

/// One owned key: its DSL path, expected shape, requirement, and the
/// description captured from the struct field's documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigKey {
    /// Dotted DSL path relative to the plugin's root block, lowerCamelCase.
    pub path: String,
    /// The value shape the key accepts.
    pub kind: KeyKind,
    /// Whether omitting the key is a configure error.
    pub required: bool,
    /// Human-readable description from the field's documentation.
    pub description: String,
}

/// A plugin's declared surface: owned keys plus named build features.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSchema {
    /// Keys this plugin owns, in declaration order.
    pub keys: Vec<ConfigKey>,
    /// Named build features (bool toggles) with their descriptions.
    pub features: BTreeMap<String, String>,
}

/// Reads the schema catalog out of a raw WebAssembly/component binary.
///
/// Returns `Ok(None)` when the artifact carries no schema section — the
/// degraded mode for plugins built before Phase 16A — and an error only
/// when the bytes are not parseable WebAssembly at all or the section is
/// malformed.
pub fn read_schema(wasm: &[u8]) -> Result<Option<PluginSchema>, String> {
    let payloads = find_section_payloads(wasm)?;
    match payloads {
        None => Ok(None),
        Some(text) => {
            let mut schema = PluginSchema::default();
            let mut lines = text.lines();
            let header = lines.next().ok_or("empty schema section")?;
            if header != "ulb-config-schema 1" {
                return Err(format!(
                    "unsupported schema header {header:?}; expected \"ulb-config-schema 1\""
                ));
            }
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let mut parts = line.split('\t');
                match parts.next() {
                    Some("k") => {
                        let path = parts.next().unwrap_or_default();
                        let kind = parts.next().and_then(KeyKind::parse).ok_or_else(|| {
                            format!("malformed schema line (unknown kind): {line:?}")
                        })?;
                        let required = match parts.next() {
                            Some("req") => true,
                            Some("opt") => false,
                            _ => {
                                return Err(format!(
                                    "malformed schema line (requirement): {line:?}"
                                ));
                            }
                        };
                        let description = parts.next().unwrap_or_default().to_owned();
                        schema.keys.push(ConfigKey {
                            path: path.to_owned(),
                            kind,
                            required,
                            description,
                        });
                    }
                    Some("f") => {
                        let name = parts.next().unwrap_or_default().to_owned();
                        let description = parts.next().unwrap_or_default().to_owned();
                        schema.features.insert(name, description);
                    }
                    other => {
                        return Err(format!(
                            "malformed schema line (unknown tag {other:?}): {line:?}"
                        ));
                    }
                }
            }
            Ok(Some(schema))
        }
    }
}

/// Walks a wasm/component binary collecting every payload stored under
/// [`SCHEMA_SECTION_NAME`]. `Ok(None)` means the name never appears.
fn find_section_payloads(wasm: &[u8]) -> Result<Option<String>, String> {
    if wasm.len() < 8 || wasm[..4] != [0x00, 0x61, 0x73, 0x6d] {
        return Err("not a WebAssembly binary (bad magic)".to_owned());
    }
    let is_component = wasm[4..8] == [0x0a, 0x00, 0x01, 0x00];
    walk_sections(&wasm[8..], is_component, 0)
}

fn walk_sections(bytes: &[u8], component: bool, depth: u8) -> Result<Option<String>, String> {
    if depth > 4 {
        return Err("section nesting too deep".to_owned());
    }
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let id = bytes[cursor];
        cursor += 1;
        let (size, consumed) = read_leb_u32(bytes, cursor)?;
        cursor += consumed;
        let end = cursor + size as usize;
        if end > bytes.len() {
            return Err("truncated section".to_owned());
        }
        let payload = &bytes[cursor..end];
        if id == 0 {
            // Custom section: u32 name length, then the UTF-8 name, then
            // this section's own payload.
            let (name_len, name_consumed) = read_leb_u32(payload, 0)?;
            let name_start = name_consumed;
            let name_end = name_start + name_len as usize;
            if name_end > payload.len() {
                return Err("truncated custom-section name".to_owned());
            }
            if &payload[name_start..name_end] == SCHEMA_SECTION_NAME.as_bytes() {
                return match std::str::from_utf8(&payload[name_end..]) {
                    Ok(text) => Ok(Some(text.to_owned())),
                    Err(_) => Err("schema section is not valid UTF-8".to_owned()),
                };
            }
        } else if component && id == 1 && depth == 0 {
            // A component wraps its core code module as section id 1;
            // descend into it to reach the plugin's own custom sections.
            if let found @ Some(_) = walk_sections(payload, false, depth + 1)? {
                return Ok(found);
            }
        }
        cursor = end;
    }
    Ok(None)
}

fn read_leb_u32(bytes: &[u8], start: usize) -> Result<(u32, usize), String> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    let mut consumed = 0usize;
    loop {
        let byte = *bytes
            .get(start + consumed)
            .ok_or("truncated LEB128 number")?;
        consumed += 1;
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, consumed));
        }
        shift += 7;
        if shift > 28 {
            return Err("LEB128 number too long".to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal core module: header plus one custom section.
    fn core_module_with_custom(name: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let mut section = vec![name.len() as u8];
        section.extend_from_slice(name);
        section.extend_from_slice(payload);
        out.push(0); // custom section id
        out.push(section.len() as u8);
        out.extend_from_slice(&section);
        out
    }

    #[test]
    fn reads_a_schema_from_a_plain_core_module() {
        let wasm = core_module_with_custom(
            SCHEMA_SECTION_NAME.as_bytes(),
            b"ulb-config-schema 1\nk greeting\tstring\treq\tThe text.\nf shout\tLoud.\n",
        );
        let schema = read_schema(&wasm).expect("parses").expect("has schema");
        assert_eq!(schema.keys.len(), 1);
        assert_eq!(schema.keys[0].path, "greeting");
        assert_eq!(schema.keys[0].kind, KeyKind::String);
        assert!(schema.keys[0].required);
        assert_eq!(schema.keys[0].description, "The text.");
        assert_eq!(
            schema.features.get("shout").map(String::as_str),
            Some("Loud.")
        );
    }

    #[test]
    fn descends_into_a_component_wrapped_core_module() {
        let inner = core_module_with_custom(
            SCHEMA_SECTION_NAME.as_bytes(),
            b"ulb-config-schema 1\nk depth\tint\topt\t\n",
        );
        // Component preamble magic/version, then the core module as a
        // section with id 1.
        let mut component = vec![0x00, 0x61, 0x73, 0x6d, 0x0a, 0x00, 0x01, 0x00];
        component.push(1);
        component.push(inner.len() as u8);
        component.extend_from_slice(&inner);
        let schema = read_schema(&component)
            .expect("parses")
            .expect("has schema");
        assert_eq!(schema.keys[0].path, "depth");
        assert!(!schema.keys[0].required);
    }

    #[test]
    fn absence_is_not_an_error() {
        let plain_core = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(read_schema(&plain_core), Ok(None));
    }

    #[test]
    fn other_custom_sections_are_ignored() {
        let wasm = core_module_with_custom(b"some:other-section", b"whatever");
        assert_eq!(read_schema(&wasm), Ok(None));
    }

    #[test]
    fn non_wasm_input_is_an_error_not_none() {
        assert!(read_schema(b"not wasm at all").is_err());
    }

    #[test]
    fn unsupported_header_version_is_rejected() {
        let wasm =
            core_module_with_custom(SCHEMA_SECTION_NAME.as_bytes(), b"ulb-config-schema 999\n");
        let error = read_schema(&wasm).expect_err("unknown version");
        assert!(error.contains("unsupported schema header"), "{error}");
    }
}
