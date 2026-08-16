# Task graph and incremental executor (`crates/uliab/src/task.rs`)

The task engine is the execution half of the build. Plugins register
tasks during `configure`; the driver collects every module's tasks into
one graph and runs it.

## The task model

```
Task { name, module, inputs, outputs, depends_on, action }
```

- `name` is module-scoped; `module` disambiguates tasks from different
  plugins.
- `inputs` / `outputs` are file paths the engine fingerprints. Outputs
  declared by one task may be consumed by another task's inputs, but
  `depends_on` edges are the only thing that orders execution.
- `action` is one of:
  - `Copy { from, to }`
  - `WriteFile { to, contents }` — `contents` participates in the fingerprint
  - `RunTool { tool, args, cwd }` — `tool` must be a parsed
    `AllowlistedTool` (`cp cat mkdir echo javac kotlinc jar java aapt2`) that the
    plugin declared in its manifest. `aapt2` is the only tool not resolved
    from the `PATH`: the action names the Android SDK `build-tools`
    directory as `args[0]` and the engine runs `<dir>/aapt2` with the rest.

## Graph construction

`TaskGraph` partitions tasks into **waves** by longest-chain depth, in
registration-stable order, so that within a wave every task is runnable in
parallel. Two construction errors:

- `depends_on` naming an unknown task → error naming the reference;
- a cycle → error that reports the cycle path.

## Execution

The `Executor` runs waves on a worker pool sized to the machine. Within a
wave, tasks start in registration order so output is deterministic across
runs. After the first failure no new tasks are scheduled, but in-wave
siblings that already started are allowed to finish.

A panicking task action is caught per-worker and surfaced as a
`TaskFailure` — the panic never unwinds the whole build, and it can never
be recorded as a silent success.

## UP-TO-DATE semantics

A task is skipped only when **both** hold:

1. its own fingerprint matches the previously recorded run, **and**
2. every dependency is itself UP-TO-DATE this build.

The second clause matters: a dependency that reran may have changed
outputs the task did not declare as inputs, so dependents must rerun too.
The executor tracks the set of tasks classified UP-TO-DATE this run and
skips a task only if all of its dependencies are in that set.

## Fingerprints

Fingerprints are SHA-256 content hashes folded over:

- the module's config block,
- all `.ulb` files that fed the module model,
- the resolved dependency graph,
- each declared file input's content (an absent input hashes as absent),
- the registering plugin version,
- the tool version.

The fingerprint is computed once per task per wave — once for
classification and reused on success — and the action is rendered
canonically so a literal change in a `WriteFile` action is a fingerprint
change.

## Persistence

Recorded fingerprints persist to `<project>/.uliab/state.json` through a
format-versioned store. If the store's format version is unknown, the
store rebuilds from scratch rather than mis-skipping tasks.

## Contract with the ABI

The executor knows nothing about Java, Kotlin, or Android. Those are
plugin concerns; the engine only executes `Task` records. The allowed
tool set and the closed action set in `abi.md` are the entire vocabulary.
