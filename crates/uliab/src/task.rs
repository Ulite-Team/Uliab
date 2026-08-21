//! The build task graph and its execution engine (ARCHITECTURE.md §4).
//!
//! Tasks are the unit of work a plugin registers during configuration
//! (§4.1): a name scoped to the module that registered it, declared input
//! and output files, `dependsOn` edges to sibling tasks in the same
//! module, and a closed action — copying a file, writing a file with
//! fixed contents, or running one of the core allowlisted tools (§3.5). The graph is a DAG; [`TaskGraph`]
//! partitions it into waves of tasks that can run concurrently (§4.2) and
//! detects dependency cycles.
//!
//! Execution is incremental (§10): [`Executor::execute`] fingerprints each
//! task's inputs plus the configuration it came from, and skips a task
//! whose fingerprint matches the last recorded successful run. The engine
//! is target-agnostic — it schedules and runs actions but never inspects a
//! module model, so the same executor drives every toolchain plugin.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One of the small tools the core allowlists for `run_tool` actions
/// (ARCHITECTURE.md §3.5). The set is closed and core-owned; a plugin
/// declares which tools it needs and the host refuses to load a plugin
/// that requests anything outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AllowlistedTool {
    /// `cp` — copy a file.
    Copy,
    /// `cat` — write a file to standard output.
    Cat,
    /// `mkdir` — create a directory.
    Mkdir,
    /// `echo` — write a line to standard output.
    Echo,
    /// `javac` — compile Java sources.
    Javac,
    /// `kotlinc` — compile Kotlin sources.
    Kotlinc,
    /// `jar` — archive class files.
    Jar,
    /// `java` — run a compiled JVM program.
    Java,
    /// `aapt2` — Android asset packaging tool. The binary is not on the
    /// `PATH`; it lives under an Android SDK `build-tools` directory, so
    /// the action names that directory as its first argument and the host
    /// resolves `<dir>/aapt2` for it.
    Aapt2,
    /// `apksigner` — APK signing tool. Like `aapt2`, the binary lives
    /// under an Android SDK `build-tools` directory and the action carries
    /// that directory as its first argument.
    Apksigner,
}

impl AllowlistedTool {
    /// The binary the tool invokes.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "cp",
            Self::Cat => "cat",
            Self::Mkdir => "mkdir",
            Self::Echo => "echo",
            Self::Javac => "javac",
            Self::Kotlinc => "kotlinc",
            Self::Jar => "jar",
            Self::Java => "java",
            Self::Aapt2 => "aapt2",
            Self::Apksigner => "apksigner",
        }
    }

    /// Resolves a tool by its binary name, or `None` when the name is not
    /// allowlisted.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "cp" => Some(Self::Copy),
            "cat" => Some(Self::Cat),
            "mkdir" => Some(Self::Mkdir),
            "echo" => Some(Self::Echo),
            "javac" => Some(Self::Javac),
            "kotlinc" => Some(Self::Kotlinc),
            "jar" => Some(Self::Jar),
            "java" => Some(Self::Java),
            "aapt2" => Some(Self::Aapt2),
            "apksigner" => Some(Self::Apksigner),
            _ => None,
        }
    }
}

/// The closed set of actions a task may perform (§4.1). A task carries
/// exactly one action; there is no way to chain several in a single task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAction {
    /// Copy one file to another, creating the destination directory if it
    /// does not exist.
    Copy {
        /// Source file.
        from: PathBuf,
        /// Destination file.
        to: PathBuf,
    },
    /// Write a file with fixed contents, creating the destination directory
    /// if it does not exist. The contents are part of the task's fingerprint,
    /// so changing them re-runs the task.
    WriteFile {
        /// Destination file.
        to: PathBuf,
        /// Exact contents to write.
        contents: String,
    },
    /// Run an allowlisted tool with arguments in a working directory.
    RunTool {
        /// The tool to invoke; it must be on the executor's allowlist.
        tool: AllowlistedTool,
        /// Arguments passed to the tool, in order.
        args: Vec<String>,
        /// Working directory for the invocation.
        cwd: PathBuf,
    },
}

/// A unit of work registered by a plugin during configuration (§4.1).
///
/// A task is identified by its `module` and `name`; `depends_on` names
/// refer to sibling tasks in the same module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Task name, e.g. `compileReleaseKotlin`.
    pub name: String,
    /// Module that registered the task; part of the task's identity.
    pub module: String,
    /// Files whose contents feed the task's fingerprint (§10). A declared
    /// input may not exist yet when the task is scheduled — it is then
    /// fingerprinted as absent and will run.
    pub inputs: Vec<PathBuf>,
    /// Files the task is expected to produce (informational; the engine
    /// does not act on them).
    pub outputs: Vec<PathBuf>,
    /// Names of tasks in the same module that must succeed before this one
    /// runs. References an undefined task at schedule time.
    pub depends_on: Vec<String>,
    /// The single action this task performs.
    pub action: TaskAction,
}

impl Task {
    /// Builds a task with no dependencies.
    pub fn leaf(
        name: impl Into<String>,
        module: impl Into<String>,
        inputs: Vec<PathBuf>,
        outputs: Vec<PathBuf>,
        action: TaskAction,
    ) -> Self {
        Self {
            name: name.into(),
            module: module.into(),
            inputs,
            outputs,
            depends_on: Vec::new(),
            action,
        }
    }
}

/// A graph-level problem that prevents a build from being scheduled
/// (§4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// A task's `dependsOn` references a task that was never registered.
    UnknownDependency {
        /// The task holding the bad reference.
        task: String,
        /// The referenced but undefined task.
        dep: String,
    },
    /// `dependsOn` edges form a cycle; `path` is the cycle without the
    /// closing edge, e.g. `["a", "b"]` for `a -> b -> a`.
    Cycle(Vec<String>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDependency { task, dep } => {
                write!(f, "task '{task}' depends on undefined task '{dep}'")
            }
            Self::Cycle(path) => {
                let chain = path.join(" -> ");
                let closed = path
                    .first()
                    .map(|node| format!(" -> {node}"))
                    .unwrap_or_default();
                write!(f, "task dependency cycle: {chain}{closed}")
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// The set of tasks registered for one build, keyed by `module::name`.
///
/// Declaration order is preserved so waves, failure reports, and therefore
/// build output are reproducible across runs (§4.2).
#[derive(Debug, Clone, Default)]
pub struct TaskGraph {
    tasks: BTreeMap<String, Task>,
    /// Registration order, keyed as `module::name`.
    order: Vec<String>,
}

impl TaskGraph {
    /// Creates an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a task.
    ///
    /// # Errors
    ///
    /// Returns an error when a task with the same `module::name` is
    /// already registered.
    pub fn register(&mut self, task: Task) -> Result<(), String> {
        let key = format!("{}::{}", task.module, task.name);
        if self.tasks.contains_key(&key) {
            return Err(format!("task '{key}' is already registered"));
        }
        self.order.push(key.clone());
        self.tasks.insert(key, task);
        Ok(())
    }

    /// The number of registered tasks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the graph has no tasks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Looks up a task by module and name.
    #[must_use]
    pub fn get(&self, module: &str, name: &str) -> Option<&Task> {
        self.tasks.get(&format!("{module}::{name}"))
    }

    /// Iterates tasks in registration order.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.order.iter().map(|key| &self.tasks[key])
    }

    /// Iterates tasks mutably in registration order.
    pub fn tasks_mut(&mut self) -> impl Iterator<Item = &mut Task> {
        self.tasks.values_mut()
    }

    /// Partitions the graph into waves (ARCHITECTURE.md §4.2): every task
    /// lands in the wave one greater than the longest dependency chain
    /// feeding it, so tasks in the same wave are mutually independent and
    /// all dependencies of wave `n` are in earlier waves. Tasks within a
    /// wave are ordered by registration order.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::UnknownDependency`] or [`GraphError::Cycle`]
    /// when the graph cannot be scheduled.
    pub fn waves(&self) -> Result<Vec<Vec<&Task>>, GraphError> {
        let mut depths: BTreeMap<String, usize> = BTreeMap::new();
        let mut state: BTreeMap<String, u8> = BTreeMap::new();
        let mut stack: Vec<String> = Vec::new();
        for key in &self.order {
            self.explore(key, &mut depths, &mut state, &mut stack)?;
        }
        let max_depth = depths.values().copied().max().unwrap_or(0);
        let mut waves: Vec<Vec<&Task>> = vec![Vec::new(); max_depth + 1];
        for key in &self.order {
            waves[depths[key]].push(&self.tasks[key]);
        }
        Ok(waves)
    }

    /// DFS depth assignment with cycle detection. `state` marks a node as
    /// unvisited (absent), in progress (`1`, on the DFS stack), or done
    /// (`2`); revisiting an in-progress node is a cycle.
    fn explore(
        &self,
        key: &str,
        depths: &mut BTreeMap<String, usize>,
        state: &mut BTreeMap<String, u8>,
        stack: &mut Vec<String>,
    ) -> Result<usize, GraphError> {
        match state.get(key) {
            Some(1) => {
                let start = stack.iter().position(|node| node == key).unwrap_or(0);
                return Err(GraphError::Cycle(stack[start..].to_vec()));
            }
            Some(2) => return Ok(depths[key]),
            _ => {}
        }
        let Some(task) = self.tasks.get(key) else {
            return Err(GraphError::UnknownDependency {
                task: stack.last().cloned().unwrap_or_default(),
                dep: key.to_owned(),
            });
        };
        state.insert(key.to_owned(), 1);
        stack.push(key.to_owned());
        let mut max_dep = 0;
        for dep in &task.depends_on {
            let dep_key = format!("{}::{}", task.module, dep);
            max_dep = max_dep.max(self.explore(&dep_key, depths, state, stack)?);
        }
        stack.pop();
        state.insert(key.to_owned(), 2);
        let depth = if task.depends_on.is_empty() {
            0
        } else {
            max_dep + 1
        };
        depths.insert(key.to_owned(), depth);
        Ok(depth)
    }

    /// Resolves cross-plugin dependency references in `depends_on` entries.
    ///
    /// A cross-plugin dep uses the format `"plugin_name:task_name"` (single
    /// colon). The `plugin_task_names` mapping tells which task names each
    /// plugin registered. After resolution, every `"plugin:task"` entry is
    /// replaced with the bare `task_name` — since all tasks in the graph
    /// share the same module, `waves()` resolves the bare name correctly.
    ///
    /// Returns an error when a cross-plugin dep references a plugin that
    /// has no matching task name in the graph.
    pub fn resolve_cross_plugin_deps(
        &mut self,
        plugin_task_names: &HashMap<String, HashSet<String>>,
    ) -> Result<(), GraphError> {
        for key in self.order.clone() {
            if let Some(task) = self.tasks.get_mut(&key) {
                for dep in &mut task.depends_on {
                    if let Some(colon_pos) = dep.find(':') {
                        let plugin_name = &dep[..colon_pos];
                        let task_name = &dep[colon_pos + 1..];
                        if let Some(tasks) = plugin_task_names.get(plugin_name) {
                            if tasks.contains(task_name) {
                                *dep = task_name.to_owned();
                            } else {
                                return Err(GraphError::UnknownDependency {
                                    task: key,
                                    dep: dep.clone(),
                                });
                            }
                        } else {
                            return Err(GraphError::UnknownDependency {
                                task: key,
                                dep: dep.clone(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Inputs to a task's fingerprint beyond its own input files (ARCHITECTURE
/// §10): the plugin version that registered the task and a
/// caller-computed, content-addressed hash of the configuration the task
/// came from. The executor only needs this context; computing the
/// configuration hash happens during configuration, which the engine does
/// not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintContext {
    /// Version of the plugin the task was registered by.
    pub plugin_version: String,
    /// Hash of the resolved module config and `.ulb` sources the task
    /// depends on.
    pub config_hash: String,
}

/// Recorded fingerprints of successful task runs (ARCHITECTURE.md §10,
/// §9 step 11). The executor records a fingerprint when a task succeeds
/// and consults it to skip tasks whose inputs have not changed.
///
/// Persisted as a JSON file (`.uliab/state.json` in a project) with a
/// format version so a future engine can ignore state it does not
/// understand instead of corrupting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintStore {
    state_path: PathBuf,
    fingerprints: BTreeMap<String, String>,
}

/// On-disk shape of a [`FingerprintStore`]; bumped when the key or hash
/// scheme changes incompatibly.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    format: u32,
    fingerprints: BTreeMap<String, String>,
}

impl FingerprintStore {
    /// Loads the store from `state_path`. A missing file yields an empty
    /// store (first build); a file with an unsupported format version is
    /// treated as empty rather than errored, so a downgraded tool chain
    /// rebuilds instead of mis-skipping.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but is not valid JSON state.
    pub fn load(state_path: impl Into<PathBuf>) -> Result<Self, String> {
        let state_path = state_path.into();
        let fingerprints = match std::fs::read_to_string(&state_path) {
            Ok(text) => {
                let state: StateFile = serde_json::from_str(&text).map_err(|error| {
                    format!("{}: invalid state file: {error}", state_path.display())
                })?;
                if state.format == 1 {
                    state.fingerprints
                } else {
                    BTreeMap::new()
                }
            }
            Err(_) => BTreeMap::new(),
        };
        Ok(Self {
            state_path,
            fingerprints,
        })
    }

    /// Whether a task, identified by `module::name`, was last run with
    /// exactly `fingerprint`.
    #[must_use]
    pub fn is_up_to_date(&self, key: &str, fingerprint: &str) -> bool {
        self.fingerprints
            .get(key)
            .is_some_and(|recorded| recorded == fingerprint)
    }

    /// Records that a task succeeded with `fingerprint`.
    pub fn record(&mut self, key: &str, fingerprint: &str) {
        self.fingerprints
            .insert(key.to_owned(), fingerprint.to_owned());
    }

    /// Persists the store to `state_path`, creating parent directories.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be serialized or written.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        let state = StateFile {
            format: 1,
            fingerprints: self.fingerprints.clone(),
        };
        let json = serde_json::to_string_pretty(&state)
            .map_err(|error| format!("serializing state: {error}"))?;
        std::fs::write(&self.state_path, json)
            .map_err(|error| format!("writing {}: {error}", self.state_path.display()))
    }
}

/// The outcome of one build run (§4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildResult {
    /// Tasks whose action actually ran.
    pub ran: usize,
    /// Tasks skipped because their inputs were unchanged since the last
    /// successful run.
    pub up_to_date: usize,
    /// Tasks never started because an earlier dependency failed.
    pub skipped: usize,
    /// The first task failure, when any task failed.
    pub failure: Option<TaskFailure>,
}

/// Why one task failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFailure {
    /// The failed task, as `module::name`.
    pub task: String,
    /// The error the task's action returned.
    pub error: String,
}

/// A function that runs one task's action.
type Runner = Arc<dyn Fn(&Task) -> Result<(), String> + Send + Sync>;

/// Executes a [`TaskGraph`] incrementally over a [`FingerprintStore`].
///
/// The executor owns the tool allowlist (§3.5) and the worker count (one
/// per logical CPU). A run partitions the graph into waves, executes each
/// wave concurrently, and stops scheduling after the first failure —
/// though tasks already running in the failed task's own wave finish
/// (§4.2). Successful runs record fingerprints so the next run skips
/// unchanged tasks; the caller persists the store afterwards.
pub struct Executor {
    allowlist: HashSet<AllowlistedTool>,
    workers: usize,
    runner: Runner,
}

impl Executor {
    /// Creates an executor allowing exactly `allowlist` and running with
    /// one worker per logical CPU.
    pub fn new(allowlist: impl IntoIterator<Item = AllowlistedTool>) -> Self {
        let allowlist: HashSet<AllowlistedTool> = allowlist.into_iter().collect();
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let runner: Runner = {
            let allowlist = allowlist.clone();
            Arc::new(move |task| run_action(&allowlist, task))
        };
        Self {
            allowlist,
            workers,
            runner,
        }
    }

    /// Executes `graph`, recording successful fingerprints into `store`.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::UnknownDependency`] or [`GraphError::Cycle`]
    /// when the graph cannot be scheduled. Individual task failures are
    /// not errors; they are reported in [`BuildResult::failure`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::path::PathBuf;
    /// use uliab::task::{Executor, FingerprintContext, FingerprintStore, Task, TaskAction, TaskGraph};
    ///
    /// let root = std::env::temp_dir().join(format!(
    ///     "uliab-execute-{}", std::process::id()
    /// ));
    /// std::fs::create_dir_all(&root).unwrap();
    /// std::fs::write(root.join("in.txt"), "hello").unwrap();
    ///
    /// let mut graph = TaskGraph::new();
    /// graph.register(Task::leaf(
    ///     "stage",
    ///     "app",
    ///     vec![root.join("in.txt")],
    ///     vec![root.join("stage.txt")],
    ///     TaskAction::Copy {
    ///         from: root.join("in.txt"),
    ///         to: root.join("stage.txt"),
    ///     },
    /// )).unwrap();
    /// graph.register(Task {
    ///     depends_on: vec!["stage".to_owned()],
    ///     ..Task::leaf(
    ///         "bundle",
    ///         "app",
    ///         vec![root.join("stage.txt")],
    ///         vec![root.join("out.txt")],
    ///         TaskAction::Copy {
    ///             from: root.join("stage.txt"),
    ///             to: root.join("out.txt"),
    ///         },
    ///     )
    /// }).unwrap();
    ///
    /// let ctx = FingerprintContext {
    ///     plugin_version: "0.1.0".to_owned(),
    ///     config_hash: "abc123".to_owned(),
    /// };
    /// let mut store = FingerprintStore::load(root.join("state.json")).unwrap();
    ///
    /// let first = Executor::new([]).execute(&graph, &ctx, &mut store).unwrap();
    /// assert_eq!(first.ran, 2);
    /// assert_eq!(std::fs::read(root.join("out.txt")).unwrap(), b"hello");
    ///
    /// // The second run has nothing to do: inputs and config are unchanged.
    /// let second = Executor::new([]).execute(&graph, &ctx, &mut store).unwrap();
    /// assert_eq!(second.ran, 0);
    /// assert_eq!(second.up_to_date, 2);
    /// ```
    pub fn execute(
        &self,
        graph: &TaskGraph,
        ctx: &FingerprintContext,
        store: &mut FingerprintStore,
    ) -> Result<BuildResult, GraphError> {
        let waves = graph.waves()?;
        let mut result = BuildResult::default();
        // Tasks classified UP-TO-DATE this run. A task is skipped only when
        // every dependency was also UP-TO-DATE (§4.2 step 3), so a task that
        // re-ran this build forces its dependents to re-run too even when
        // their own fingerprints still match.
        let mut up_to_date_keys: BTreeSet<String> = BTreeSet::new();

        for wave in &waves {
            if result.failure.is_some() {
                result.skipped += wave.len();
                continue;
            }

            let mut to_run: Vec<(&Task, String)> = Vec::new();
            let mut up_to_date: Vec<String> = Vec::new();
            for task in wave {
                let key = format!("{}::{}", task.module, task.name);
                let deps_up_to_date = task
                    .depends_on
                    .iter()
                    .all(|dep| up_to_date_keys.contains(&format!("{}::{dep}", task.module)));
                let fingerprint = fingerprint(task, ctx);
                if deps_up_to_date && store.is_up_to_date(&key, &fingerprint) {
                    up_to_date_keys.insert(key.clone());
                    up_to_date.push(key);
                } else {
                    to_run.push((task, fingerprint));
                }
            }

            let outcomes = self.run_in_parallel(&to_run);
            for ((task, fingerprint), outcome) in to_run.iter().zip(outcomes) {
                let key = format!("{}::{}", task.module, task.name);
                match outcome {
                    Ok(()) => {
                        store.record(&key, fingerprint);
                        result.ran += 1;
                    }
                    Err(error) => {
                        if result.failure.is_none() {
                            result.failure = Some(TaskFailure { task: key, error });
                        }
                    }
                }
            }
            result.up_to_date += up_to_date.len();
        }
        Ok(result)
    }

    /// The tools this executor allows.
    #[must_use]
    pub fn allowlist(&self) -> &HashSet<AllowlistedTool> {
        &self.allowlist
    }

    /// Runs one task through the configured runner (used by tests).
    #[cfg(test)]
    fn run_task(&self, task: &Task) -> Result<(), String> {
        (self.runner)(task)
    }

    /// Runs tasks on a fixed worker pool, returning one outcome per task
    /// in input order.
    fn run_in_parallel(&self, tasks: &[(&Task, String)]) -> Vec<Result<(), String>> {
        if tasks.len() == 1 {
            return vec![self.run_caught(tasks[0].0)];
        }
        let workers = self.workers.min(tasks.len()).max(1);
        let queue = Mutex::new(VecDeque::from_iter(0..tasks.len()));
        let results = Mutex::new(vec![None; tasks.len()]);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = queue.lock().unwrap().pop_front();
                        let Some(index) = index else { break };
                        let outcome = self.run_caught(tasks[index].0);
                        results.lock().unwrap()[index] = Some(outcome);
                    }
                });
            }
        });
        results
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .into_iter()
            .map(|outcome| outcome.unwrap_or_else(|| Err("task produced no result".to_owned())))
            .collect()
    }

    /// Runs one task, converting a panicking runner into an error so a
    /// buggy action is a task failure rather than a build crash.
    fn run_caught(&self, task: &Task) -> Result<(), String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.runner)(task))) {
            Ok(outcome) => outcome,
            Err(payload) => Err(format!(
                "task runner panicked: {}",
                panic_message(payload.as_ref())
            )),
        }
    }

    /// Test-only constructor with an injected runner and worker count.
    #[cfg(test)]
    fn with_runner(allowlist: Vec<AllowlistedTool>, workers: usize, runner: Runner) -> Self {
        Self {
            allowlist: allowlist.into_iter().collect(),
            workers,
            runner,
        }
    }
}

/// Runs a task's action against the allowlist.
fn run_action(allowlist: &HashSet<AllowlistedTool>, task: &Task) -> Result<(), String> {
    match &task.action {
        TaskAction::Copy { from, to } => {
            if !from.exists() {
                return Err(format!("copy source '{}' does not exist", from.display()));
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("creating {}: {error}", parent.display()))?;
            }
            std::fs::copy(from, to)
                .map_err(|error| format!("copy {} -> {}: {error}", from.display(), to.display()))?;
            Ok(())
        }
        TaskAction::WriteFile { to, contents } => {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("creating {}: {error}", parent.display()))?;
            }
            std::fs::write(to, contents)
                .map_err(|error| format!("writing {}: {error}", to.display()))?;
            Ok(())
        }
        TaskAction::RunTool { tool, args, cwd } => {
            if !allowlist.contains(tool) {
                return Err(format!("tool '{}' is not on the allowlist", tool.as_str()));
            }
            let (binary, tool_args) = resolve_tool(*tool, args)?;
            let output = std::process::Command::new(&binary)
                .args(tool_args)
                .current_dir(cwd)
                .output()
                .map_err(|error| format!("running '{binary}': {error}"))?;
            if !output.status.success() {
                return Err(format!(
                    "'{binary}' exited with {}: stdout: {} stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
            Ok(())
        }
    }
}

/// Resolves the binary a `run-tool` action invokes, paired with the
/// arguments that belong to it.
///
/// Most tools run by their bare name on the `PATH`. `aapt2` is different:
/// it ships inside an Android SDK `build-tools` directory rather than on
/// the `PATH`, so the action carries that directory as its first argument
/// and the host resolves `<dir>/aapt2`, stripping the directory from the
/// arguments the tool actually receives.
fn resolve_tool(tool: AllowlistedTool, args: &[String]) -> Result<(String, &[String]), String> {
    match tool {
        AllowlistedTool::Aapt2 => {
            let dir = args.first().ok_or_else(|| {
                "tool 'aapt2' requires the build-tools directory as its first argument".to_owned()
            })?;
            let binary = std::path::PathBuf::from(dir)
                .join(format!("aapt2{}", std::env::consts::EXE_SUFFIX));
            if !binary.exists() {
                return Err(format!(
                    "aapt2 binary '{}' does not exist",
                    binary.display()
                ));
            }
            Ok((binary.display().to_string(), &args[1..]))
        }
        AllowlistedTool::Apksigner => {
            let dir = args.first().ok_or_else(|| {
                "tool 'apksigner' requires the build-tools directory as its first argument"
                    .to_owned()
            })?;
            let base = std::path::PathBuf::from(dir);
            let candidates: Vec<String> = if cfg!(windows) {
                vec!["apksigner.bat".to_owned(), "apksigner.exe".to_owned()]
            } else {
                vec!["apksigner".to_owned()]
            };
            let mut found = None;
            for name in &candidates {
                let path = base.join(name);
                if path.exists() {
                    found = Some(path);
                    break;
                }
            }
            let binary = found.ok_or_else(|| {
                format!(
                    "apksigner binary not found in '{}' (looked for {})",
                    dir,
                    candidates.join(", ")
                )
            })?;
            Ok((binary.display().to_string(), &args[1..]))
        }
        _ => Ok((tool.as_str().to_owned(), args)),
    }
}

/// Content-addressed fingerprint of a task's inputs (ARCHITECTURE §10):
/// the plugin version, the configuration hash, the contents of each
/// declared input file (missing inputs hash as absent), a directory input
/// hashed as its tree of relative paths and file contents, and a rendering
/// of the action itself so a changed action forces a rerun.
fn fingerprint(task: &Task, ctx: &FingerprintContext) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ctx.plugin_version.as_bytes());
    hasher.update([0u8]);
    hasher.update(ctx.config_hash.as_bytes());
    hasher.update([0u8]);
    for input in &task.inputs {
        match std::fs::metadata(input) {
            Ok(metadata) if metadata.is_dir() => hash_directory(&mut hasher, input),
            _ => match streamed_digest(input) {
                Some(digest) => {
                    hasher.update([1u8]);
                    hasher.update(digest);
                }
                None => hasher.update([0u8]),
            },
        }
    }
    hasher.update([0u8]);
    hasher.update(render_action(&task.action).as_bytes());
    hex(&hasher.finalize())
}

/// Hashes the tree under `dir` into `hasher`: each file's path relative
/// to `dir` (sorted for determinism) followed by its content digest.
/// Adding, removing, or editing a file under the tree therefore changes
/// the fingerprint. An unreadable tree hashes as absent, matching how a
/// missing input file is treated.
fn hash_directory(hasher: &mut Sha256, dir: &Path) {
    let mut files = Vec::new();
    if collect_dir_files(dir, &mut files).is_err() {
        hasher.update([0u8]);
        return;
    }
    files.sort();
    hasher.update([2u8]);
    for file in files {
        let relative = file.strip_prefix(dir).unwrap_or(&file);
        match streamed_digest(&file) {
            Some(digest) => {
                hasher.update([1u8]);
                hasher.update(relative.as_os_str().as_encoded_bytes());
                hasher.update([0u8]);
                hasher.update(digest);
            }
            None => hasher.update([0u8]),
        }
    }
}

/// Hashes the contents of `file` with SHA-256 in chunks, so hashing a
/// large input never buffers it in memory. Returns `None` when the file
/// cannot be read, matching how [`fingerprint`] treats an unreadable
/// input. The digest is chunk-invariant, so it equals the one-shot
/// `Sha256::digest` of the same content byte for byte.
fn streamed_digest(file: &Path) -> Option<[u8; 32]> {
    let mut reader = BufReader::new(std::fs::File::open(file).ok()?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hasher.finalize().into())
}

/// Collects every file (not directory) under `dir`, recursively.
fn collect_dir_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_dir_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// A canonical rendering of an action, stable across runs so equal actions
/// hash equally.
fn render_action(action: &TaskAction) -> String {
    match action {
        TaskAction::Copy { from, to } => {
            format!("copy {} -> {}", from.display(), to.display())
        }
        TaskAction::WriteFile { to, contents } => {
            format!("write {} with <<{contents}>>", to.display())
        }
        TaskAction::RunTool { tool, args, cwd } => {
            format!(
                "tool {} args [{}] cwd {}",
                tool.as_str(),
                args.join(" "),
                cwd.display()
            )
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Recovers a readable message from a caught panic payload, for reporting
/// in a [`TaskFailure`].
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn copy_task(name: &str, from: &str, to: &str) -> Task {
        Task::leaf(
            name,
            "app",
            Vec::new(),
            Vec::new(),
            TaskAction::Copy {
                from: PathBuf::from(from),
                to: PathBuf::from(to),
            },
        )
    }

    fn ctx(config_hash: &str) -> FingerprintContext {
        FingerprintContext {
            plugin_version: "0.1.0".to_owned(),
            config_hash: config_hash.to_owned(),
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uliab-task-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut graph = TaskGraph::new();
        graph.register(copy_task("a", "x", "y")).unwrap();
        let error = graph
            .register(copy_task("a", "x", "y"))
            .expect_err("duplicate");
        assert!(error.contains("app::a"));
    }

    #[test]
    fn same_name_in_different_modules_coexist() {
        let mut graph = TaskGraph::new();
        for module in ["one", "two"] {
            graph
                .register(Task::leaf(
                    "a",
                    module,
                    vec![],
                    vec![],
                    TaskAction::Copy {
                        from: PathBuf::from("x"),
                        to: PathBuf::from("y"),
                    },
                ))
                .unwrap();
        }
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn waves_respect_longest_chain_and_registration_order() {
        let mut graph = TaskGraph::new();
        // chain c -> b -> a plus an independent d; registration order is
        // intentionally not topological.
        graph.register(copy_task("a", "a", "b")).unwrap();
        graph.register(copy_task("d", "d", "e")).unwrap();
        graph
            .register(Task {
                depends_on: vec!["a".to_owned()],
                ..copy_task("b", "b", "c")
            })
            .unwrap();
        graph
            .register(Task {
                depends_on: vec!["b".to_owned()],
                ..copy_task("c", "c", "d")
            })
            .unwrap();

        let waves = graph.waves().expect("acyclic");
        let names: Vec<Vec<String>> = waves
            .iter()
            .map(|wave| wave.iter().map(|task| task.name.clone()).collect())
            .collect();
        assert_eq!(names, vec![vec!["a", "d"], vec!["b"], vec!["c"]]);
    }

    #[test]
    fn cycle_reports_the_path() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["b".to_owned()],
                ..copy_task("a", "a", "b")
            })
            .unwrap();
        graph
            .register(Task {
                depends_on: vec!["a".to_owned()],
                ..copy_task("b", "b", "a")
            })
            .unwrap();
        let error = graph.waves().expect_err("cycle");
        let GraphError::Cycle(path) = &error else {
            panic!("expected a cycle, got {error:?}");
        };
        assert_eq!(path, &["app::a".to_owned(), "app::b".to_owned()]);
        assert!(error.to_string().contains("app::a -> app::b -> app::a"));
    }

    #[test]
    fn unknown_dependency_is_a_graph_error() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["ghost".to_owned()],
                ..copy_task("a", "a", "b")
            })
            .unwrap();
        let error = graph.waves().expect_err("undefined dep");
        let GraphError::UnknownDependency { task, dep } = &error else {
            panic!("expected unknown dependency, got {error:?}");
        };
        assert_eq!(task, "app::a");
        assert_eq!(dep, "app::ghost");
    }

    #[test]
    fn fingerprint_is_deterministic_and_sensitive_to_input_and_config() {
        let root = temp_dir("fingerprint");
        let input = root.join("in.txt");
        std::fs::write(&input, "v1").unwrap();
        let task = Task::leaf(
            "a",
            "app",
            vec![input.clone()],
            vec![],
            TaskAction::Copy {
                from: input.clone(),
                to: root.join("out.txt"),
            },
        );

        let first = fingerprint(&task, &ctx("cfg"));
        assert_eq!(first, fingerprint(&task, &ctx("cfg")));
        assert_ne!(first, fingerprint(&task, &ctx("other")));

        std::fs::write(&input, "v2").unwrap();
        assert_ne!(first, fingerprint(&task, &ctx("cfg")));
    }

    #[test]
    fn directory_input_fingerprint_tracks_the_tree() {
        let root = temp_dir("fingerprint-dir");
        let dir = root.join("res");
        std::fs::create_dir_all(dir.join("layout")).unwrap();
        std::fs::create_dir_all(dir.join("values")).unwrap();
        std::fs::write(dir.join("layout/activity.xml"), "<a/>").unwrap();
        std::fs::write(dir.join("values/strings.xml"), "<r/>").unwrap();
        let task = Task::leaf(
            "res",
            "app",
            vec![dir.clone()],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Echo,
                args: vec!["x".to_owned()],
                cwd: root.clone(),
            },
        );

        let baseline = fingerprint(&task, &ctx("cfg"));
        assert_eq!(baseline, fingerprint(&task, &ctx("cfg")));

        std::fs::write(dir.join("layout/activity.xml"), "<b/>").unwrap();
        assert_ne!(baseline, fingerprint(&task, &ctx("cfg")));

        std::fs::write(dir.join("layout/activity.xml"), "<a/>").unwrap();
        let restored = fingerprint(&task, &ctx("cfg"));
        assert_eq!(baseline, restored);

        std::fs::write(dir.join("values/extra.xml"), "<e/>").unwrap();
        assert_ne!(baseline, fingerprint(&task, &ctx("cfg")));

        std::fs::remove_file(dir.join("values/extra.xml")).unwrap();
        assert_eq!(baseline, fingerprint(&task, &ctx("cfg")));
    }

    /// Deterministic, non-trivial content that crosses many 16 KiB
    /// hashing chunks (1 MiB of pseudo-random bytes).
    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn streamed_digest_matches_one_shot_hashing() {
        let root = temp_dir("streamed-digest");
        let file = root.join("big.bin");
        let content = pseudo_random_bytes(1024 * 1024);
        std::fs::write(&file, &content).unwrap();
        let one_shot = Sha256::digest(&content);
        let streamed = streamed_digest(&file).expect("reads");
        assert_eq!(streamed.as_slice(), one_shot.as_slice());
        assert_eq!(streamed, streamed_digest(&file).expect("reads again"));
    }

    #[test]
    fn directory_fingerprint_tracks_bytes_across_chunk_boundaries() {
        let root = temp_dir("fingerprint-boundary");
        let dir = root.join("res");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.bin");
        let mut content = pseudo_random_bytes(1024 * 1024);
        std::fs::write(&file, &content).unwrap();
        let task = Task::leaf(
            "res",
            "app",
            vec![dir.clone()],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Echo,
                args: vec!["x".to_owned()],
                cwd: root.clone(),
            },
        );

        let baseline = fingerprint(&task, &ctx("cfg"));
        assert_eq!(baseline, fingerprint(&task, &ctx("cfg")));

        content[16 * 1024 + 7] ^= 0xff;
        std::fs::write(&file, &content).unwrap();
        assert_ne!(baseline, fingerprint(&task, &ctx("cfg")));

        content[16 * 1024 + 7] ^= 0xff;
        std::fs::write(&file, &content).unwrap();
        assert_eq!(baseline, fingerprint(&task, &ctx("cfg")));
    }

    #[test]
    fn aapt2_runs_the_build_tools_binary_without_the_dir_argument() {
        let root = temp_dir("aapt2");
        let build_tools = root.join("build-tools/36.0.0");
        std::fs::create_dir_all(&build_tools).unwrap();
        let script = "#!/bin/sh\necho \"$*\" > \"$PWD/aapt2-args.txt\"\n";
        std::fs::write(build_tools.join("aapt2"), script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                build_tools.join("aapt2"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let task = Task::leaf(
            "res",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Aapt2,
                args: vec![
                    build_tools.display().to_string(),
                    "compile".to_owned(),
                    "--dir".to_owned(),
                    "res".to_owned(),
                ],
                cwd: work.clone(),
            },
        );
        Executor::new([AllowlistedTool::Aapt2])
            .run_task(&task)
            .expect("aapt2 runs");
        assert_eq!(
            std::fs::read_to_string(work.join("aapt2-args.txt")).unwrap(),
            "compile --dir res\n"
        );
    }

    #[test]
    fn aapt2_requires_the_build_tools_dir_as_first_argument() {
        let task = Task::leaf(
            "res",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Aapt2,
                args: vec![],
                cwd: PathBuf::from("."),
            },
        );
        let error = Executor::new([AllowlistedTool::Aapt2])
            .run_task(&task)
            .expect_err("no dir argument");
        assert!(error.contains("build-tools directory"));
    }

    #[test]
    fn aapt2_refuses_a_missing_build_tools_dir() {
        let root = temp_dir("aapt2-missing");
        let task = Task::leaf(
            "res",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Aapt2,
                args: vec![root.join("nowhere").display().to_string()],
                cwd: root.clone(),
            },
        );
        let error = Executor::new([AllowlistedTool::Aapt2])
            .run_task(&task)
            .expect_err("missing dir");
        assert!(error.contains("does not exist"));
    }

    #[test]
    fn execute_copies_and_skips_unchanged_tasks() {
        let root = temp_dir("execute");
        std::fs::write(root.join("in.txt"), "hello").unwrap();
        let mut graph = TaskGraph::new();
        graph
            .register(Task::leaf(
                "stage",
                "app",
                vec![root.join("in.txt")],
                vec![root.join("stage.txt")],
                TaskAction::Copy {
                    from: root.join("in.txt"),
                    to: root.join("stage.txt"),
                },
            ))
            .unwrap();
        graph
            .register(Task {
                depends_on: vec!["stage".to_owned()],
                ..Task::leaf(
                    "bundle",
                    "app",
                    vec![root.join("stage.txt")],
                    vec![root.join("out.txt")],
                    TaskAction::Copy {
                        from: root.join("stage.txt"),
                        to: root.join("out.txt"),
                    },
                )
            })
            .unwrap();

        let mut store = FingerprintStore::load(root.join("state.json")).unwrap();
        let first = Executor::new([])
            .execute(&graph, &ctx("cfg"), &mut store)
            .expect("schedules");
        assert_eq!((first.ran, first.up_to_date, first.skipped), (2, 0, 0));
        assert_eq!(std::fs::read(root.join("out.txt")).unwrap(), b"hello");

        let second = Executor::new([])
            .execute(&graph, &ctx("cfg"), &mut store)
            .expect("schedules");
        assert_eq!((second.ran, second.up_to_date), (0, 2));

        store.save().unwrap();
        let mut reloaded = FingerprintStore::load(root.join("state.json")).unwrap();
        let third = Executor::new([])
            .execute(&graph, &ctx("cfg"), &mut reloaded)
            .expect("schedules");
        assert_eq!((third.ran, third.up_to_date), (0, 2));
    }

    #[test]
    fn changing_an_input_reruns_only_affected_tasks() {
        let root = temp_dir("rerun");
        std::fs::write(root.join("in.txt"), "v1").unwrap();
        let mut graph = TaskGraph::new();
        graph
            .register(Task::leaf(
                "a",
                "app",
                vec![root.join("in.txt")],
                vec![],
                TaskAction::Copy {
                    from: root.join("in.txt"),
                    to: root.join("a.out"),
                },
            ))
            .unwrap();
        graph
            .register(Task::leaf(
                "b",
                "app",
                vec![root.join("in.txt")],
                vec![],
                TaskAction::Copy {
                    from: root.join("in.txt"),
                    to: root.join("b.out"),
                },
            ))
            .unwrap();

        let mut store = FingerprintStore::load(root.join("state.json")).unwrap();
        Executor::new([])
            .execute(&graph, &ctx("cfg"), &mut store)
            .unwrap();

        // Both tasks read in.txt; changing it must rerun both.
        std::fs::write(root.join("in.txt"), "v2").unwrap();
        let result = Executor::new([])
            .execute(&graph, &ctx("cfg"), &mut store)
            .unwrap();
        assert_eq!((result.ran, result.up_to_date), (2, 0));
    }

    #[test]
    fn dependent_reruns_when_its_dependency_reran() {
        let root = temp_dir("deprerun");
        std::fs::write(root.join("in.txt"), "v1").unwrap();
        let mut graph = TaskGraph::new();
        // d reads in.txt and writes d.out; e depends on d and declares no
        // inputs, so e's fingerprint never changes.
        graph
            .register(Task::leaf(
                "d",
                "app",
                vec![root.join("in.txt")],
                vec![root.join("d.out")],
                TaskAction::Copy {
                    from: root.join("in.txt"),
                    to: root.join("d.out"),
                },
            ))
            .unwrap();
        graph
            .register(Task {
                depends_on: vec!["d".to_owned()],
                ..Task::leaf(
                    "e",
                    "app",
                    vec![],
                    vec![root.join("e.out")],
                    TaskAction::Copy {
                        from: root.join("d.out"),
                        to: root.join("e.out"),
                    },
                )
            })
            .unwrap();

        let executor = Executor::new([]);
        let ctx = ctx("cfg");
        let mut store = FingerprintStore::load(root.join("state.json")).unwrap();
        executor
            .execute(&graph, &ctx, &mut store)
            .expect("schedules");

        // d's input changes: d reruns, and e must rerun too even though its
        // own fingerprint is unchanged (§4.2 step 3).
        std::fs::write(root.join("in.txt"), "v2").unwrap();
        let result = executor
            .execute(&graph, &ctx, &mut store)
            .expect("schedules");
        assert_eq!((result.ran, result.up_to_date), (2, 0));
        assert_eq!(std::fs::read(root.join("e.out")).unwrap(), b"v2");
    }

    #[test]
    fn a_panicking_runner_is_a_task_failure_not_a_crash() {
        let runner: Runner = Arc::new(|_| panic!("runner exploded"));
        let executor = Executor::with_runner(vec![], 2, runner);
        let mut graph = TaskGraph::new();
        for name in ["a", "b"] {
            graph.register(copy_task(name, "x", "y")).unwrap();
        }
        let result = executor
            .execute(
                &graph,
                &ctx("cfg"),
                &mut FingerprintStore::load(temp_dir("panic").join("s.json")).unwrap(),
            )
            .expect("schedules");
        let failure = result.failure.expect("task failed");
        assert!(failure.error.contains("runner exploded"));
    }

    #[test]
    fn failure_skips_dependents_but_wave_siblings_finish() {
        let root = temp_dir("failure");
        std::fs::write(root.join("src.txt"), "x").unwrap();
        let mut graph = TaskGraph::new();
        // a fails (missing source); c is an independent sibling in the same
        // wave and must still run; b depends on a and is skipped.
        graph
            .register(Task::leaf(
                "a",
                "app",
                vec![],
                vec![],
                TaskAction::Copy {
                    from: root.join("missing.txt"),
                    to: root.join("a.out"),
                },
            ))
            .unwrap();
        graph
            .register(Task::leaf(
                "c",
                "app",
                vec![],
                vec![],
                TaskAction::Copy {
                    from: root.join("src.txt"),
                    to: root.join("c.out"),
                },
            ))
            .unwrap();
        graph
            .register(Task {
                depends_on: vec!["a".to_owned()],
                ..Task::leaf(
                    "b",
                    "app",
                    vec![],
                    vec![],
                    TaskAction::Copy {
                        from: root.join("src.txt"),
                        to: root.join("b.out"),
                    },
                )
            })
            .unwrap();

        let result = Executor::new([])
            .execute(
                &graph,
                &ctx("cfg"),
                &mut FingerprintStore::load(root.join("state.json")).unwrap(),
            )
            .expect("schedules");
        let failure = result.failure.expect("a failed");
        assert_eq!(failure.task, "app::a");
        assert_eq!((result.ran, result.up_to_date, result.skipped), (1, 0, 1));
        assert!(root.join("c.out").exists());
        assert!(!root.join("b.out").exists());
    }

    #[test]
    fn run_tool_denied_when_not_allowlisted() {
        let task = Task::leaf(
            "a",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Cat,
                args: vec!["missing.txt".to_owned()],
                cwd: PathBuf::from("."),
            },
        );
        let error = Executor::new([]).run_task(&task).expect_err("denied");
        assert!(error.contains("allowlist"));
    }

    #[test]
    fn run_tool_surfaces_nonzero_exit() {
        let root = temp_dir("tool");
        let task = Task::leaf(
            "a",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Cat,
                args: vec!["does-not-exist.txt".to_owned()],
                cwd: root.clone(),
            },
        );
        let error = Executor::new([AllowlistedTool::Cat])
            .run_task(&task)
            .expect_err("cat fails");
        assert!(error.contains("exited with"));
    }

    #[test]
    fn copy_creates_the_destination_directory() {
        let root = temp_dir("copydir");
        std::fs::write(root.join("in.txt"), "data").unwrap();
        let task = Task::leaf(
            "a",
            "app",
            vec![],
            vec![],
            TaskAction::Copy {
                from: root.join("in.txt"),
                to: root.join("nested/deep/out.txt"),
            },
        );
        Executor::new([]).run_task(&task).expect("copy ok");
        assert_eq!(
            std::fs::read(root.join("nested/deep/out.txt")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn write_file_creates_the_destination_directory_and_contents() {
        let root = temp_dir("writefile");
        let task = Task::leaf(
            "gen",
            "app",
            vec![],
            vec![root.join("generated/Runner.java")],
            TaskAction::WriteFile {
                to: root.join("generated/Runner.java"),
                contents: "public final class Runner {}".to_owned(),
            },
        );
        Executor::new([]).run_task(&task).expect("write ok");
        assert_eq!(
            std::fs::read(root.join("generated/Runner.java")).unwrap(),
            b"public final class Runner {}"
        );
    }

    #[test]
    fn write_file_reruns_when_contents_change() {
        let root = temp_dir("write-rerun");
        let to = root.join("gen/Runner.java");
        let task = |contents: &str| {
            Task::leaf(
                "gen",
                "app",
                vec![],
                vec![to.clone()],
                TaskAction::WriteFile {
                    to: to.clone(),
                    contents: contents.to_owned(),
                },
            )
        };
        let first = fingerprint(&task("v1"), &ctx("cfg"));
        assert_eq!(first, fingerprint(&task("v1"), &ctx("cfg")));
        assert_ne!(first, fingerprint(&task("v2"), &ctx("cfg")));
        assert_ne!(first, fingerprint(&task("v1"), &ctx("other")));
    }

    #[test]
    fn wave_runs_concurrently_up_to_the_worker_count() {
        let running = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let runner: Runner = Arc::new({
            let running = Arc::clone(&running);
            let max_concurrent = Arc::clone(&max_concurrent);
            move |_| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let executor = Executor::with_runner(vec![], 3, runner);

        let mut graph = TaskGraph::new();
        for name in ["a", "b", "c", "d"] {
            graph.register(copy_task(name, "x", "y")).unwrap();
        }
        let result = executor
            .execute(
                &graph,
                &ctx("cfg"),
                &mut FingerprintStore::load(temp_dir("par").join("s.json")).unwrap(),
            )
            .expect("schedules");
        assert_eq!(result.ran, 4);
        assert!(max_concurrent.load(Ordering::SeqCst) >= 2);
    }
}
