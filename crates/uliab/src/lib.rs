//! The `uliab` build-tool binary.
//!
//! The initial ABI scope is loading a plugin component and running its
//! `run` entry point (ARCHITECTURE.md §3.3). The build-graph driver is
//! future work; this library exposes the host side so the CLI and the
//! integration tests share it.

pub mod host;
