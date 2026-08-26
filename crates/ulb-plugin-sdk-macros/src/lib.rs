//! Procedural macros for the ulb plugin SDK.
//!
//! [`UlbConfig`] derives, from a single config struct, both a typed
//! deserializer out of the module-config JSON and the plugin's declared
//! key/feature catalog embedded into the compiled artifact as a
//! `ulb:config-schema` custom section (ARCHITECTURE.md §3.8). One
//! expansion produces both outputs, so the executable behavior and its
//! machine-readable description cannot drift apart.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Field, Fields, Lit, Meta, parse_macro_input};

/// Derives the plugin's embedded config-schema catalog.
///
/// The struct's fields become the keys the plugin owns. Field names are
/// converted to lowerCamelCase DSL names unless renamed — pair this derive
/// with `#[derive(serde::Deserialize)]` + `#[serde(rename_all =
/// "camelCase")]` and the two agree automatically; per-field renames must
/// be declared in BOTH attributes (`#[serde(rename = "x")]` and
/// `#[ulb(rename = "x")]`). Doc comments become descriptions unless
/// overridden with `#[ulb(desc = "text")]`. An `Option<T>` field is
/// optional; every other field is required. A `bool` field annotated with
/// `#[ulb(feature)]` is additionally published as a build feature.
///
/// Deserialization is NOT generated: derive `serde::Deserialize` on the
/// same struct and call `serde_json::from_value` in `configure`. Keeping
/// deserialization on serde's battle-tested path leaves this macro a
/// pure metadata generator — nothing here can break a build.
#[proc_macro_derive(UlbConfig, attributes(ulb))]
pub fn derive_ulb_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct FieldSpec {
    dsl_name: String,
    kind: &'static str,
    optional: bool,
    description: String,
    is_feature: bool,
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let type_name = &input.ident;
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "UlbConfig does not support generic structs",
        ));
    }
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    &input.ident,
                    "UlbConfig requires a struct with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "UlbConfig can only be derived for structs",
            ));
        }
    };

    let mut specs = Vec::new();
    for field in fields {
        specs.push(field_spec(field)?);
    }

    // The catalog is a small line-oriented document so any consumer — host,
    // LSP, or a human with `strings` — can read it without a JSON parser:
    //   k <path>\t<int|string|bool|list|block>\t<req|opt>\t<description>
    //   f <name>\t<description>
    let mut schema_lines = vec!["ulb-config-schema 1".to_owned()];
    for spec in &specs {
        let requirement = if spec.optional { "opt" } else { "req" };
        schema_lines.push(format!(
            "k\t{}\t{}\t{}\t{}",
            spec.dsl_name, spec.kind, requirement, spec.description
        ));
        if spec.is_feature {
            schema_lines.push(format!("f\t{}\t{}", spec.dsl_name, spec.description));
        }
    }
    let schema_text = schema_lines.join("\n");

    // The custom section static must state its exact byte length in its
    // type, so the length is computed here at expansion time.
    let schema_len = schema_text.len();
    let schema_bytes = proc_macro2::Literal::byte_string(schema_text.as_bytes());

    let impl_tokens = quote! {
        #[automatically_derived]
        impl #type_name {
            /// The plugin's declared key/feature catalog in the
            /// `ulb-config-schema 1` line format (ARCHITECTURE.md §3.8).
            pub const ULB_CONFIG_SCHEMA: &'static str = #schema_text;

        }

        #[cfg(target_arch = "wasm32")]
        #[link_section = "ulb:config-schema"]
        static _ULB_CONFIG_SCHEMA_SECTION: [u8; #schema_len] = *#schema_bytes;
    };

    let debug_note = format!("EXPANSION[{}]: {}", type_name, impl_tokens);

    // Temporary diagnostics channel; removed once the derive is proven.
    eprintln!("{}", debug_note);

    Ok(impl_tokens)
}

fn field_spec(field: &Field) -> syn::Result<FieldSpec> {
    let mut rename: Option<String> = None;
    let mut override_desc: Option<String> = None;
    let mut is_feature = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("ulb") {
            continue;
        }
        let Meta::List(list) = &attr.meta else {
            return Err(syn::Error::new_spanned(attr, "malformed #[ulb] attribute"));
        };
        let nested = list.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        for meta in nested {
            match &meta {
                Meta::Path(path) if path.is_ident("feature") => is_feature = true,
                Meta::NameValue(nv) if nv.path.is_ident("rename") => {
                    rename = Some(expect_string(&nv.value)?)
                }
                Meta::NameValue(nv) if nv.path.is_ident("desc") => {
                    override_desc = Some(expect_string(&nv.value)?)
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unknown ulb attribute; expected feature, rename, or desc",
                    ));
                }
            }
        }
    }

    let description = match override_desc {
        Some(text) => text,
        None => doc_description(field),
    };

    let (optional, inner_ty) = unwrap_option(&field.ty);
    let visible = inner_ty.as_ref().unwrap_or(&field.ty);
    let kind = classify(visible)?;

    let dsl_name = match rename {
        Some(text) => text,
        None => {
            let ident_name = field.ident.as_ref().expect("named field").to_string();
            lower_camel_case(&ident_name)
        }
    };

    Ok(FieldSpec {
        dsl_name,
        kind,
        optional,
        description,
        is_feature,
    })
}

fn expect_string(value: &Expr) -> syn::Result<String> {
    if let Expr::Lit(lit) = value
        && let Lit::Str(text) = &lit.lit
    {
        return Ok(text.value());
    }
    Err(syn::Error::new_spanned(
        value,
        "#[ulb] values must be string literals",
    ))
}

fn doc_description(field: &Field) -> String {
    let mut parts: Vec<String> = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(lit) = &nv.value
            && let Lit::Str(text) = &lit.lit
        {
            parts.push(text.value().trim().to_owned());
        }
    }
    parts.join(" ")
}

fn unwrap_option(ty: &syn::Type) -> (bool, Option<syn::Type>) {
    if let syn::Type::Path(path) = ty
        && path.qself.is_none()
        && path.path.segments.len() == 1
        && path.path.segments[0].ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &path.path.segments[0].arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (true, Some(inner.clone()));
    }
    (false, None)
}

fn classify(ty: &syn::Type) -> syn::Result<&'static str> {
    let syn::Type::Path(path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "UlbConfig fields must be simple paths (String, bool, ints, Vec<String>, or a nested UlbConfig struct)",
        ));
    };
    if path.qself.is_none() && path.path.segments.len() == 1 {
        let name = path.path.segments.last().unwrap().ident.to_string();
        match name.as_str() {
            "String" => return Ok("string"),
            "bool" => return Ok("bool"),
            "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
                return Ok("int");
            }
            "Vec" => {
                let is_string_list = matches!(
                    &path.path.segments.last().unwrap().arguments,
                    syn::PathArguments::AngleBracketed(args)
                        if matches!(
                            args.args.first(),
                            Some(syn::GenericArgument::Type(inner))
                                if type_path_is_string(inner)
                        )
                );
                return if is_string_list {
                    Ok("list")
                } else {
                    Err(syn::Error::new_spanned(
                        ty,
                        "only Vec<String> lists are supported by UlbConfig",
                    ))
                };
            }
            _ => {}
        }
    }
    // Anything else must be a nested struct deriving UlbConfig; the
    // generated `from_config` call makes a non-conforming type a compile
    // error at the use site.
    Ok("block")
}

fn type_path_is_string(ty: &syn::Type) -> bool {
    matches!(
        ty,
        syn::Type::Path(path)
            if path.qself.is_none()
                && path.path.is_ident("String")
    )
}

fn lower_camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper_next = false;
    for ch in name.chars() {
        if ch == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}
