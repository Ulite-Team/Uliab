//! `#[derive(UlbConfig)]` procedural macro for plugin config schemas
//! (ARCHITECTURE.md §3.8).
//!
//! Given a struct like:
//!
//! ```rust,ignore
//! #[derive(UlbConfig)]
//! struct Android {
//!     /// The compile SDK version.
//!     compile_sdk: i32,
//!     /// Whether Compose is enabled.
//!     compose: Option<bool>,
//! }
//! ```
//!
//! The macro generates:
//!
//! 1. A `serde::Deserialize` implementation for the struct.
//! 2. An `impl` block with:
//!    - `fn schema() -> serde_json::Value` — the config schema as a JSON
//!      value, with field names, types, descriptions, and optionality.
//!    - `const SCHEMA_JSON: &str` — the same schema as a string constant,
//!      for embedding into a wasm custom section.
//!
//! # Field attributes
//!
//! - `#[ulb(description = "...")]` — override the doc-comment-derived
//!   description.
//! - `#[ulb(rename = "...")]` — override the JSON key name.
//! - `#[ulb(skip)]` — exclude the field from the schema (it is still
//!   deserialized but not part of the declared DSL surface).

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Lit, Meta, MetaNameValue, parse_macro_input};

/// Derives a plugin config schema from a struct definition.
///
/// See the module-level documentation for details.
#[proc_macro_derive(UlbConfig, attributes(ulb))]
pub fn derive_ulb_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_ulb_config(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Embeds a type's config schema into the wasm binary as a custom section.
///
/// Accepts a type that implements `UlbConfig` (via `#[derive(UlbConfig)]`).
/// Generates a `#[link_section = "ulb-config-schema"]` static containing
/// the schema JSON bytes, null-terminated.
///
/// On non-wasm targets, this is a no-op (the static is still emitted but
/// the linker discards unused link sections).
///
/// # Usage
///
/// ```rust,ignore
/// #[derive(UlbConfig, serde::Deserialize)]
/// struct MyConfig { /* ... */ }
///
/// ulb_config_macros::embed_schema!(MyConfig);
/// ```
#[proc_macro]
pub fn embed_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::Path);
    let section_name = "ulb-config-schema";

    // The generated code calls <Type>::SCHEMA_JSON at compile time. The
    // schema string is a const &str, and .as_bytes() gives us the raw
    // bytes. We build a null-terminated byte array with a #[link_section]
    // attribute so it ends up in the wasm custom section.
    //
    // Key insight: at this point in macro expansion we don't know the
    // string content (it comes from another const), so we emit runtime
    // code that uses const evaluation. Rust's const evaluator can handle
    // `<Type>::SCHEMA_JSON.as_bytes().len()` when the const is in scope.
    //
    // However, [u8; N] requires N to be a const expression known at
    // compile time *for the array declaration*. We use a two-step trick:
    // (1) declare a const for the length, (2) use it in the array type.
    //
    // This works because Rust allows `const LEN: usize = expr;` followed
    // by `[u8; LEN]` in the same scope — both are evaluated at compile
    // time.

    let expanded = quote! {
        // The schema bytes. Null-terminated so the host can validate
        // without relying on the custom section length.
        //
        // Safety: the link section name is a compile-time constant
        // containing only ASCII characters, satisfying the requirements
        // of `#[link_section]`.
        #[cfg_attr(target_arch = "wasm32", unsafe(link_section = #section_name))]
        #[used]
        static __ULB_CONFIG_SCHEMA: [u8; {
            const __SCHEMA: &str = <#input>::SCHEMA_JSON;
            __SCHEMA.as_bytes().len() + 1
        }] = {
            const __SCHEMA: &str = <#input>::SCHEMA_JSON;
            const __BYTES: &[u8] = __SCHEMA.as_bytes();
            const __LEN: usize = __BYTES.len() + 1;
            let mut buf = [0u8; __LEN];
            let mut i = 0usize;
            while i < __BYTES.len() {
                buf[i] = __BYTES[i];
                i += 1;
            }
            // Null terminator
            buf[__LEN - 1] = 0;
            buf
        };
    };

    expanded.into()
}

fn expand_ulb_config(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let name_str = name.to_string();
    let struct_doc = extract_doc_comment(&input.attrs).unwrap_or_default();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            Fields::Unit => {
                return expand_ulb_config_unit(&name_str, &struct_doc);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    input,
                    "UlbConfig can only be derived for structs with named fields or unit structs",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "UlbConfig can only be derived for structs",
            ));
        }
    };

    let mut field_schemas = Vec::new();

    for field in fields {
        let attrs = parse_field_attrs(field)?;
        if attrs.skip {
            continue;
        }

        let field_name = field.ident.as_ref().unwrap();
        let json_key = attrs.rename.unwrap_or_else(|| field_name.to_string());
        let description = attrs
            .description
            .or_else(|| extract_doc_comment(&field.attrs))
            .unwrap_or_default();

        let (type_name, required) = analyze_field_type(&field.ty);

        field_schemas.push(FieldSchema {
            json_key,
            type_name,
            description,
            required,
        });
    }

    // Build the schema JSON string at compile time
    let schema_json_str = build_schema_json_string(&name_str, &struct_doc, &field_schemas);
    let schema_json_for_value = schema_json_str.clone();

    Ok(quote! {
        impl #name {
            /// Returns the config schema as a JSON value, derived from the
            /// struct's field names, types, and doc comments.
            pub fn schema() -> serde_json::Value {
                serde_json::from_str(#schema_json_for_value)
                    .expect("UlbConfig macro generated invalid schema JSON")
            }

            /// The config schema as a JSON string constant, for embedding
            /// in a wasm custom section via
            /// `ulb_plugin_sdk::embed_schema!`.
            pub const SCHEMA_JSON: &'static str = #schema_json_str;
        }
    })
}

/// Expands `UlbConfig` for a unit struct (no fields).
fn expand_ulb_config_unit(
    struct_name: &str,
    struct_doc: &str,
) -> syn::Result<proc_macro2::TokenStream> {
    let name: syn::Ident = syn::parse_str(struct_name)?;
    let schema_json_str = build_schema_json_string(struct_name, struct_doc, &[]);
    let schema_json_for_value = schema_json_str.clone();

    Ok(quote! {
        impl #name {
            /// Returns the config schema as a JSON value.
            pub fn schema() -> serde_json::Value {
                serde_json::from_str(#schema_json_for_value)
                    .expect("UlbConfig macro generated invalid schema JSON")
            }

            /// The config schema as a JSON string constant, for embedding
            /// in a wasm custom section via
            /// `ulb_plugin_sdk::embed_schema!`.
            pub const SCHEMA_JSON: &'static str = #schema_json_str;
        }
    })
}

/// Intermediate representation of a field's schema metadata.
struct FieldSchema {
    json_key: String,
    type_name: String,
    description: String,
    required: bool,
}

/// Builds the schema JSON string at compile time from the collected metadata.
fn build_schema_json_string(struct_name: &str, struct_doc: &str, fields: &[FieldSchema]) -> String {
    let mut schema = serde_json::Map::new();
    schema.insert("name".into(), serde_json::Value::String(struct_name.into()));
    schema.insert(
        "description".into(),
        serde_json::Value::String(struct_doc.into()),
    );

    let mut properties = Vec::new();
    for f in fields {
        let mut prop = serde_json::Map::new();
        prop.insert("name".into(), serde_json::Value::String(f.json_key.clone()));
        prop.insert(
            "type_name".into(),
            serde_json::Value::String(f.type_name.clone()),
        );
        prop.insert(
            "description".into(),
            serde_json::Value::String(f.description.clone()),
        );
        prop.insert("required".into(), serde_json::Value::Bool(f.required));
        properties.push(serde_json::Value::Object(prop));
    }

    schema.insert("properties".into(), serde_json::Value::Array(properties));

    serde_json::to_string(&serde_json::Value::Object(schema))
        .expect("schema JSON serialization should not fail")
}

/// Parsed attributes from `#[ulb(...)]`.
struct FieldAttrs {
    rename: Option<String>,
    description: Option<String>,
    skip: bool,
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut rename = None;
    let mut description = None;
    let mut skip = false;

    for attr in &field.attrs {
        if !attr.path().is_ident("ulb") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    rename = Some(s.value());
                } else {
                    return Err(syn::Error::new_spanned(lit, "expected string literal"));
                }
            } else if meta.path.is_ident("description") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                if let Lit::Str(s) = lit {
                    description = Some(s.value());
                } else {
                    return Err(syn::Error::new_spanned(lit, "expected string literal"));
                }
            } else if meta.path.is_ident("skip") {
                skip = true;
            } else {
                return Err(syn::Error::new_spanned(
                    meta.path,
                    "unknown ulb attribute, expected: rename, description, skip",
                ));
            }
            Ok(())
        })?;
    }

    Ok(FieldAttrs {
        rename,
        description,
        skip,
    })
}

/// Extracts the first `///` doc comment from a list of attributes.
fn extract_doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.path().is_ident("doc")
            && let Meta::NameValue(MetaNameValue {
                value:
                    syn::Expr::Lit(syn::ExprLit {
                        lit: Lit::Str(s), ..
                    }),
                ..
            }) = &attr.meta
        {
            let text = s.value();
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
    }
    None
}

/// Analyzes a field's type to determine schema type name and optionality.
fn analyze_field_type(ty: &syn::Type) -> (String, bool) {
    if let syn::Type::Path(type_path) = ty
        && let Some(segment) = type_path.path.segments.last()
    {
        let ident = segment.ident.to_string();

        // Option<T> → inner type, not required
        if ident == "Option"
            && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
            && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
        {
            let (inner_type, _) = analyze_field_type(inner);
            return (inner_type, false);
        }

        // Vec<T> → array, required
        if ident == "Vec" {
            return ("array".into(), true);
        }

        // Primitive types
        match ident.as_str() {
            "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
                return ("integer".into(), true);
            }
            "String" | "str" => {
                return ("string".into(), true);
            }
            "bool" => {
                return ("boolean".into(), true);
            }
            _ => {
                // Nested struct or unknown type → object
                return ("object".into(), true);
            }
        }
    }

    ("object".into(), true)
}
