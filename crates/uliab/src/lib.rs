//! The `uliab` build-tool binary.
//!
//! The initial ABI scope is loading a plugin component and running its
//! `run` entry point (ARCHITECTURE.md §3.3), plus the plugin registry
//! client that resolves `libs.ulb` plugin coordinates to local artifacts
//! (§3.6). The build-graph driver is future work; this library exposes the
//! host and registry sides so the CLI and the integration tests share
//! them.

pub mod host;
pub mod project;
pub mod registry;
