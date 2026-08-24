//! The `uliab` build-tool binary.
//!
//! The initial ABI scope is loading a plugin component and running its
//! `run` entry point (ARCHITECTURE.md §3.3), the plugin registry client
//! that resolves `libs.ulb` plugin coordinates to local artifacts (§3.6),
//! and the build task graph that schedules and executes registered tasks
//! incrementally (ARCHITECTURE.md §4, §10). The CLI that wires project
//! configuration, plugin loading, and the executor together lives in
//! `main.rs`; this library exposes the host, registry, and task sides so
//! the CLI and the integration tests share them.

#![warn(missing_docs)]

pub mod driver;
pub mod host;
pub mod init;
pub mod maven;
pub mod project;
pub mod registry;
pub mod task;
