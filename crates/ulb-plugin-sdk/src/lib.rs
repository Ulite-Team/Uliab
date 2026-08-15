//! The plugin ABI's single source of truth.
//!
//! This crate holds `plugin.wit` — the interface between the `uliab` host
//! and a plugin component (ARCHITECTURE.md §3.2/§3.7) — plus the version
//! constants both sides check. It has no dependencies on purpose: the host
//! (`uliab`) generates its bindings from [`WIT`] with
//! `wasmtime::component::bindgen!`, and a plugin crate generates its
//! bindings from [`WIT`] with `wit_bindgen::generate!`. Because both sides
//! read the same `include_str!`ed text, the interface cannot drift between
literal://! them.
//!
//! Growing the ABI (a new entry point, a new field on `manifest`) is
//! additive-only within a major version; anything else is a major-version
//! bump with the compatibility/fallback behavior from ARCHITECTURE.md
//! §3.6.

/// The WIT document defining the `ulb-plugin` world. Both the host and
/// plugin sides generate their bindings from this exact text.
pub const WIT: &str = include_str!("../plugin.wit");

/// The plugin-ABI version this `plugin.wit` describes, `major.minor`.
///
/// A plugin reports this in its [`manifest`](crate::WIT)'s `abi-version`
/// field; the host compares it against its own [`ABI_VERSION`] before
/// calling anything else.
pub const ABI_VERSION: &str = "0.3";
