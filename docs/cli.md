# Command-line reference

The binary is `crates/uliab` (package name `uliab`). `main()` dispatches
four subcommands.

## `uliab run <plugin.wasm> [input]`

Loads a wasm artifact directly (no project, no registry), reads its
manifest, and invokes `configure`/`run`. The manifest is printed so you
can see the reported `name`, `version`, `abi-version`, and `tools`. Any
component whose `abi-version` does not match the host is refused.

This is the fastest way to exercise a plugin artifact without a project.

## `uliab plugins list`

Prints what the project's `libs.ulb` `plugins {}` table declares — the
resolved `PluginSpec` entries (name + version).

## `uliab plugins resolve`

Downloads and verifies the declared plugin builds through the registry and
prints their cached artifact paths. See
[plugin-registry.md](plugin-registry.md).

Flags (shared with `build`/`deps resolve` where relevant):

| Flag | Meaning |
|---|---|
| `--project DIR` | project directory (default: current directory) |
| `--registry SOURCE` | registry index to resolve against (default: `DEFAULT_REGISTRY`) |
| `--cache-dir DIR` | plugin cache directory (default: `~/.cache/uliab/plugins`) |

## `uliab build [--project DIR] [--registry SOURCE] [--cache-dir DIR] [--repo REPO]`

Runs the full pipeline: evaluate the project, resolve its plugins, run
each plugin's `configure`, resolve Maven deps, then execute the merged
task graph incrementally. Output is per-task `ran`/`up-to-date` lines and,
on success, a summary; a failed task fails the build with the task name
and failure payload. See [build-pipeline.md](build-pipeline.md).

## `uliab deps resolve [--project DIR] [--cache-dir DIR] [--repo REPO]`

Runs the configure phase only and prints the resolved classpath buckets
(`compile:` / `runtime:` / `testCompile:` / …). Useful for inspecting what
`deps {}` expands to before building.

## `--repo REPO`

Repeatable. Prepends a custom Maven repository (https://, file://, or a
plain path) ahead of Google Maven and Maven Central, shared by `build` and
`deps resolve` (`repos_for`). This is how offline/local-Maven projects
resolve — the KSP fixture in `ulb-plugins` relies on it.
