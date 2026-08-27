//! Plugin config schema extraction from `.wasm` custom sections
//! (ARCHITECTURE.md §3.8).
//!
//! The canonical implementation lives in the `ulb-schema` crate. This
//! module re-exports the full public API so existing callers
//! (`crate::schema::*`, `uliab::schema::*`) continue to compile
//! unchanged.

pub use ulb_schema::*;
