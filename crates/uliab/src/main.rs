//! The `uliab` command-line entry point.
//!
//! Subcommands:
//!
//! - `uliab run <plugin.wasm> [input]` — loads a component and prints the
//!   result of its `run` entry point.
//! - `uliab plugins list [--project DIR]` — prints the plugin coordinates
//!   declared in `<DIR>/libs.ulb` (default: the current directory).
//! - `uliab plugins resolve [--project DIR] [--registry SOURCE] [--cache-dir DIR]`
//!   — resolves every declared plugin against the registry, downloading
//!   into the cache on a miss (ARCHITECTURE.md §9, steps 5–6), and prints
//!   the resulting local artifact paths. `SOURCE` is a registry index URL
//!   (`https://…`) or a local `index.json` path.
//! - `uliab build [--project DIR] [--registry SOURCE] [--cache-dir DIR]`
//!   — evaluates the project, configures each declared plugin with the
//!   module model, and executes the registered task graphs incrementally
//!   (ARCHITECTURE.md §9), printing how many tasks ran versus were skipped
//!   as up-to-date.
//! - `uliab deps resolve [--project DIR] [--cache-dir DIR]` — resolves the
//!   project's `deps {}` block into a classpath (ARCHITECTURE.md §6, §7)
//!   against the default repositories, and prints the jars per bucket.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use uliab::driver::{BuildOptions, DEFAULT_REGISTRY, build_project, resolve_project_deps};
use uliab::host::PluginHost;
use uliab::maven::MavenRepo;
use uliab::project::{self, read_libs_plugins};
use uliab::registry::{Registry, RegistrySource};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("plugins") => cmd_plugins(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("deps") => cmd_deps(&args[1..]),
        _ => {
            eprintln!("usage: uliab <run|plugins|build|deps> …");
            eprintln!("  uliab run <plugin.wasm> [input]");
            eprintln!("  uliab plugins list [--project DIR]");
            eprintln!(
                "  uliab plugins resolve [--project DIR] [--registry SOURCE] [--cache-dir DIR]"
            );
            eprintln!("  uliab build [--project DIR] [--registry SOURCE] [--cache-dir DIR]");
            eprintln!("  uliab deps resolve [--project DIR] [--cache-dir DIR]");
            ExitCode::from(2)
        }
    }
}

fn cmd_run(args: &[String]) -> ExitCode {
    if args.is_empty() || args.len() > 2 {
        eprintln!("usage: uliab run <plugin.wasm> [input]");
        return ExitCode::from(2);
    }
    let path = Path::new(&args[0]);
    let input = args.get(1).map(String::as_str).unwrap_or("");

    let host = match PluginHost::new() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match host.run(path, input) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_plugins(args: &[String]) -> ExitCode {
    let sub = match args.first().map(String::as_str) {
        Some("list") => "list",
        Some("resolve") => "resolve",
        _ => {
            eprintln!("usage: uliab plugins <list|resolve> [options]");
            return ExitCode::from(2);
        }
    };
    let options = parse_options(&args[1..]);
    let Some(project_dir) = options.project_dir.as_ref() else {
        eprintln!("error: --project DIR is required");
        return ExitCode::FAILURE;
    };
    if !project_dir.is_dir() {
        eprintln!(
            "error: project directory '{}' does not exist",
            project_dir.display()
        );
        return ExitCode::FAILURE;
    }

    let libs = match read_libs_plugins(project_dir) {
        Ok(libs) => libs,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    match sub {
        "list" => {
            for spec in &libs.plugins {
                println!("{}", project::spec_label(spec));
            }
            ExitCode::SUCCESS
        }
        _ => resolve_all(&libs.plugins, &options),
    }
}

fn resolve_all(specs: &[uliab::registry::PluginSpec], options: &Options) -> ExitCode {
    if specs.is_empty() {
        eprintln!("error: libs.ulb declares no plugins");
        return ExitCode::FAILURE;
    }
    let source = match &options.registry {
        Some(source) => parse_registry_source(source),
        None => RegistrySource::Url(DEFAULT_REGISTRY.to_owned()),
    };
    let registry = Registry::new(source, options.cache_dir.clone());
    let mut failed = false;
    for spec in specs {
        match registry.resolve(spec) {
            Ok(resolved) => {
                if let Some(warning) = &resolved.warning {
                    eprintln!("warning: {warning}");
                }
                let source = if resolved.from_cache {
                    "cache"
                } else {
                    "registry"
                };
                println!(
                    "{} -> {} ({source})",
                    project::spec_label(spec),
                    resolved.path.display()
                );
            }
            Err(error) => {
                eprintln!("error: {}: {error}", project::spec_label(spec));
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Runs a full build of a project: evaluate, configure each plugin, and
/// execute the merged task graphs.
fn cmd_build(args: &[String]) -> ExitCode {
    let options = parse_options(args);
    let Some(project_dir) = options.project_dir.as_ref() else {
        eprintln!("error: --project DIR is required");
        return ExitCode::FAILURE;
    };
    if !project_dir.is_dir() {
        eprintln!(
            "error: project directory '{}' does not exist",
            project_dir.display()
        );
        return ExitCode::FAILURE;
    }

    let build_options = BuildOptions {
        registry: options.registry.as_deref().map(parse_registry_source),
        cache_dir: options.cache_dir.clone(),
        repos: None,
    };
    match build_project(project_dir, &build_options) {
        Ok(result) => {
            if let Some(failure) = &result.failure {
                eprintln!("error: task '{}' failed: {}", failure.task, failure.error);
                return ExitCode::FAILURE;
            }
            let skipped = if result.skipped > 0 {
                format!(", {} skipped", result.skipped)
            } else {
                String::new()
            };
            println!(
                "build finished: {} ran, {} up-to-date{skipped}",
                result.ran, result.up_to_date
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves the project's `deps {}` block and prints the jars of each
/// classpath bucket (ARCHITECTURE.md §6, §7).
fn cmd_deps(args: &[String]) -> ExitCode {
    let sub = match args.first().map(String::as_str) {
        Some("resolve") => "resolve",
        _ => {
            eprintln!("usage: uliab deps <resolve> [options]");
            return ExitCode::from(2);
        }
    };
    let options = parse_options(&args[1..]);
    let Some(project_dir) = options.project_dir.as_ref() else {
        eprintln!("error: --project DIR is required");
        return ExitCode::FAILURE;
    };
    if !project_dir.is_dir() {
        eprintln!(
            "error: project directory '{}' does not exist",
            project_dir.display()
        );
        return ExitCode::FAILURE;
    }
    debug_assert_eq!(sub, "resolve");

    let repos = vec![MavenRepo::Google, MavenRepo::Central];
    match resolve_project_deps(project_dir, &repos, options.cache_dir.clone()) {
        Ok(resolution) => {
            for note in &resolution.notes {
                eprintln!("note: {note}");
            }
            let buckets = [
                ("compile", &resolution.classpath.compile),
                ("runtime", &resolution.classpath.runtime),
                ("processor", &resolution.classpath.processor),
                ("testCompile", &resolution.classpath.test_compile),
                ("testRuntime", &resolution.classpath.test_runtime),
                (
                    "androidTestCompile",
                    &resolution.classpath.android_test_compile,
                ),
                (
                    "androidTestRuntime",
                    &resolution.classpath.android_test_runtime,
                ),
            ];
            for (name, jars) in buckets {
                if jars.is_empty() {
                    continue;
                }
                println!("{name}:");
                for jar in jars {
                    println!("  {}", jar.display());
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_registry_source(source: &str) -> RegistrySource {
    if source.starts_with("http://") || source.starts_with("https://") {
        RegistrySource::Url(source.to_owned())
    } else {
        RegistrySource::File(PathBuf::from(source))
    }
}

struct Options {
    project_dir: Option<PathBuf>,
    registry: Option<String>,
    cache_dir: Option<PathBuf>,
}

fn parse_options(args: &[String]) -> Options {
    let mut options = Options {
        project_dir: None,
        registry: std::env::var("ULIAB_REGISTRY").ok(),
        cache_dir: None,
    };
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--project" => {
                if let Some(value) = iter.next() {
                    options.project_dir = Some(PathBuf::from(value));
                }
            }
            "--registry" => {
                if let Some(value) = iter.next() {
                    options.registry = Some(value.clone());
                }
            }
            "--cache-dir" => {
                if let Some(value) = iter.next() {
                    options.cache_dir = Some(PathBuf::from(value));
                }
            }
            other => eprintln!("warning: ignoring unknown option '{other}'"),
        }
    }
    options
}
