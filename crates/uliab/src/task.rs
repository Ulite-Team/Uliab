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

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
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
    /// Files or directories the task is expected to produce. After a
    /// successful run the executor checks each declared output exists; a
    /// task that succeeds yet writes none of its outputs is a failure, so a
    /// silently-missing artifact is never recorded up-to-date and hashed as
    /// absent downstream.
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

/// Splits a dependency string into `(plugin_name, task_name)` when it
/// uses the cross-plugin format `"plugin_name:task_name"` (single colon).
///
/// Task names and plugin names are not allowed to contain colons; a part
/// that still carries one means the string is not a well-formed
/// cross-plugin reference, and `None` is returned so it surfaces later as
/// a resolution error instead of being split at the wrong colon. Returns
/// `None` for bare same-module dependency references.
#[must_use]
pub fn split_cross_plugin_ref(dep: &str) -> Option<(&str, &str)> {
    let colon_pos = dep.find(':')?;
    let plugin_name = &dep[..colon_pos];
    let task_name = &dep[colon_pos + 1..];
    if plugin_name.is_empty()
        || task_name.is_empty()
        || plugin_name.contains(':')
        || task_name.contains(':')
    {
        return None;
    }
    Some((plugin_name, task_name))
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

    /// Removes cross-plugin dependency references (`"plugin:task"` format)
    /// from every task's `depends_on` list, leaving same-module bare names
    /// intact. Used by the host to validate a plugin's own graph after
    /// stripping references that reference tasks from other plugins (which
    /// have not been registered yet at per-plugin validation time).
    pub fn strip_cross_plugin_refs(&mut self) {
        for key in &self.order.clone() {
            if let Some(task) = self.tasks.get_mut(key) {
                task.depends_on
                    .retain(|dep| split_cross_plugin_ref(dep).is_none());
            }
        }
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
            // A dependency containing the `::` separator is an
            // already-resolved absolute key — cross-plugin resolution
            // rewrote it to point at the provider's own module. Anything
            // else names a task in the same module.
            let dep_key = if dep.contains("::") {
                dep.clone()
            } else {
                format!("{}::{}", task.module, dep)
            };
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
    /// colon). The `locate_provider` closure decides where the provider
    /// task actually lives: tasks are registered under module labels that
    /// embed both the project module and the plugin name, so only the
    /// caller knows how to map `(consumer module, provider plugin,
    /// task name)` onto a concrete graph key such as `"app/ulite/kmp"`.
    /// Returning `None` reports an unknown dependency.
    ///
    /// After resolution every cross-plugin entry is replaced with the
    /// provider's absolute `module::task` key; [`Self::waves`] treats keys
    /// containing `::` as absolute and everything else as same-module.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::UnknownDependency`] when `locate_provider`
    /// finds no registration for a referenced plugin task.
    pub fn resolve_cross_plugin_deps(
        &mut self,
        locate_provider: &dyn Fn(&str, &str, &str) -> Option<String>,
    ) -> Result<(), GraphError> {
        let keys: Vec<String> = self.order.to_vec();
        for key in &keys {
            if let Some(task) = self.tasks.get_mut(key) {
                for dep in &mut task.depends_on {
                    if let Some((plugin_name, task_name)) = split_cross_plugin_ref(dep) {
                        let resolved = locate_provider(&task.module, plugin_name, task_name)
                            .ok_or_else(|| GraphError::UnknownDependency {
                                task: key.clone(),
                                dep: dep.clone(),
                            })?;
                        *dep = resolved;
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
    /// Returns an error when the file exists but cannot be read (e.g. a
    /// permission problem — silently rebuilding and overwriting it would
    /// hide the cause) or is not valid JSON state.
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(format!("{}: {error}", state_path.display()));
            }
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
            Arc::new(move |task| {
                run_action(&allowlist, task)?;
                verify_outputs(task)
            })
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
                let deps_up_to_date = task.depends_on.iter().all(|dep| {
                    // Absolute keys (rewritten cross-plugin refs) are used
                    // verbatim; bare names belong to the same module.
                    let dep_key = if dep.contains("::") {
                        dep.clone()
                    } else {
                        format!("{}::{dep}", task.module)
                    };
                    up_to_date_keys.contains(&dep_key)
                });
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
                        // A panicking sibling poisons the mutexes; the
                        // guards are recovered so remaining workers keep
                        // draining the queue instead of deadlocking.
                        let index = queue
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .pop_front();
                        let Some(index) = index else { break };
                        let outcome = self.run_caught(tasks[index].0);
                        results
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())[index] =
                            Some(outcome);
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

/// Verifies that every declared output of `task` exists after its action
/// ran. A tool that exits 0 yet writes none of its declared outputs (a
/// misconfigured compiler, a path mismatch) is reported as a task failure
/// rather than a silent success: recording the task up-to-date would leave
/// the missing artifact to hash as absent the next time a dependent reads
/// it.
fn verify_outputs(task: &Task) -> Result<(), String> {
    for output in &task.outputs {
        if !output.exists() {
            return Err(format!(
                "task '{}' declared output '{}' but it does not exist after the action ran",
                task.name,
                output.display()
            ));
        }
    }
    Ok(())
}

/// Resolves the binary a `run-tool` action invokes, paired with the
/// arguments that belong to it.
///
/// Most tools run by their bare name on the `PATH`. `aapt2`/`apksigner` ship
/// inside an Android SDK `build-tools` directory rather than on the `PATH`,
/// so the action carries that directory as its first argument and the host
/// resolves `<dir>/<binary>`, stripping the directory from the arguments the
/// tool actually receives.
fn resolve_tool(tool: AllowlistedTool, args: &[String]) -> Result<(String, &[String]), String> {
    match tool {
        AllowlistedTool::Aapt2 | AllowlistedTool::Apksigner => {
            let _dir = args.first().ok_or_else(|| {
                format!(
                    "tool '{}' requires the build-tools directory as its first argument",
                    tool.as_str()
                )
            })?;
            let binary = resolve_build_tools_binary(tool, args).ok_or_else(|| match tool {
                AllowlistedTool::Aapt2 => {
                    let dir = args.first().expect("dir checked above");
                    let name = format!("aapt2{}", std::env::consts::EXE_SUFFIX);
                    format!(
                        "aapt2 binary '{}' does not exist",
                        PathBuf::from(dir).join(name).display()
                    )
                }
                _ => {
                    let dir = args.first().expect("dir checked above");
                    let looked = if cfg!(windows) {
                        "apksigner.bat, apksigner.exe"
                    } else {
                        "apksigner"
                    };
                    format!(
                        "apksigner binary not found in '{}' (looked for {looked})",
                        dir
                    )
                }
            })?;
            Ok((binary.display().to_string(), &args[1..]))
        }
        _ => Ok((tool.as_str().to_owned(), args)),
    }
}

/// Resolves the concrete `build-tools` file an Android tool action names,
/// shared by the runtime spawner and the fingerprinter so the two always
/// agree on which file will run. Returns `None` when the directory argument
/// is present but no candidate exists; the caller reports the missing
/// directory argument itself.
#[must_use]
fn resolve_build_tools_binary(tool: AllowlistedTool, args: &[String]) -> Option<PathBuf> {
    let dir = args.first()?;
    if tool == AllowlistedTool::Aapt2 {
        // Lives at `<dir>/aapt2` on Unix and `<dir>/aapt2.exe` on Windows.
        let binary = PathBuf::from(dir).join(format!("aapt2{}", std::env::consts::EXE_SUFFIX));
        return binary.is_file().then_some(binary);
    }
    let names: &[&str] = if cfg!(windows) {
        &["apksigner.bat", "apksigner.exe"]
    } else {
        &["apksigner"]
    };
    names
        .iter()
        .map(|name| PathBuf::from(dir).join(name))
        .find(|path| path.is_file())
}

/// Resolves the concrete executable file a `run-tool` action will invoke,
/// for fingerprinting. Returns `Some(path)` when a real file backs the
/// action and `None` when a `PATH`-resolved tool cannot be found or the
/// action is malformed. The Android tools resolve to their concrete
/// `build-tools` file; every other tool is resolved against the current
/// `PATH`, so a switched install (a JDK or Kotlin upgrade) points at a
/// different file and changes the task's fingerprint.
#[must_use]
fn resolve_tool_binary(tool: AllowlistedTool, args: &[String]) -> Option<PathBuf> {
    match tool {
        AllowlistedTool::Aapt2 | AllowlistedTool::Apksigner => {
            resolve_build_tools_binary(tool, args)
        }
        _ => {
            // An unset `PATH` resolves to nothing (the unresolved-tool
            // marker) rather than probing the current directory, which an
            // empty-path probe would otherwise do.
            let path_var = std::env::var_os("PATH")?;
            let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
            resolve_on_path(tool.as_str(), &dirs)
        }
    }
}

/// Searches `dirs` for the first executable matching `name`, returning its
/// path, or `None` when no candidate exists. The Windows executable
/// suffixes (from `PATHEXT`) are honored so the resolved file matches the
/// one that would actually run.
///
/// Exposed to the driver so `kotlinc` discovery for the Compose compiler
/// version and the task engine's tool resolution agree on which concrete
/// file runs, rather than each drifting to its own PATH-walk semantics.
#[must_use]
pub(crate) fn resolve_on_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if cfg!(windows) {
        let exts: Vec<String> = std::env::var_os("PATHEXT")
            .map(|value| {
                std::env::split_paths(&value)
                    .filter_map(|part| {
                        part.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .map(|n| n.trim_start_matches('.').to_owned())
                    })
                    // PATHEXT entries are dot-led (`.EXE`); `extension`
                    // would drop them as hidden files, so the dot is
                    // stripped and the suffix rebuilt as `<name>.<ext>`.
                    .filter(|ext| !ext.is_empty())
                    .collect()
            })
            .unwrap_or_else(|| vec!["EXE".to_owned(), "BAT".to_owned(), "CMD".to_owned()]);
        dirs.iter()
            .flat_map(move |dir| {
                let exts = exts.clone();
                exts.into_iter()
                    .map(move |ext| dir.join(format!("{name}.{ext}")))
            })
            .find(|path| path.is_file())
    } else {
        dirs.iter()
            .map(move |dir| dir.join(name))
            .filter(|path| path.is_file())
            .find(|path| executable_on_unix(path.as_path()))
    }
}

/// Whether a file is executable, checked on Unix by the owner/group/other
/// execute bits. On non-Unix there is no portable bit to test, so any file
/// is treated as executable and resolution relies on the `is_file` filter.
#[cfg(unix)]
fn executable_on_unix(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Non-Unix counterpart of [`executable_on_unix`]: every file passes.
#[cfg(not(unix))]
fn executable_on_unix(_path: &Path) -> bool {
    true
}

/// Content-addressed fingerprint of a task's inputs (ARCHITECTURE §10):
/// the plugin version, the configuration hash, the contents of each
/// declared input file (missing inputs hash as absent), a directory input
/// hashed as its tree of relative paths and file contents, a rendering
/// of the action itself so a changed action forces a rerun, and — for
/// `run-tool` actions — the resolved executable's path and content digest
/// so a switched tool install invalidates the task.
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
    if let TaskAction::RunTool { tool, args, .. } = &task.action {
        hash_run_tool_binary(&mut hasher, *tool, args);
    }
    hex(&hasher.finalize())
}

/// Hashes the executable a `run-tool` action invokes into `hasher`, so a
/// switched install (a JDK or Kotlin upgrade) changes the fingerprint and
/// reruns the task. The binary is identified by its resolved path followed
/// by its content digest; a `PATH`-resolved tool that cannot currently be
/// found, or a malformed Android action, contributes a fixed "unresolved"
/// marker — there is no file to hash, and the run would fail at execution
/// time anyway.
fn hash_run_tool_binary(hasher: &mut Sha256, tool: AllowlistedTool, args: &[String]) {
    match resolve_tool_binary(tool, args) {
        Some(binary) => {
            hasher.update([3u8]);
            hasher.update(binary.as_os_str().as_encoded_bytes());
            hasher.update([0u8]);
            match streamed_digest(&binary) {
                Some(digest) => hasher.update(digest),
                None => hasher.update([0u8]),
            }
        }
        None => hasher.update(b"unresolved-tool"),
    }
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
///
/// Symlinked directories are not followed: an input tree is hashed by its
/// own contents, and a symlink pointing at an ancestor would otherwise
/// recurse forever.
fn collect_dir_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_dir_files(&path, out)?;
        } else if file_type.is_symlink() {
            // A symlinked file contributes its target's bytes; a symlinked
            // directory is skipped rather than descended into. Resolve the
            // link once and classify the target.
            let target_type = std::fs::metadata(&path).map(|m| m.is_dir());
            match target_type {
                Ok(false) | Err(_) => out.push(path),
                Ok(true) => {}
            }
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

    #[test]
    fn run_tool_fingerprint_tracks_the_resolved_binary_contents() {
        let root = temp_dir("tool-fp");
        let build_tools = root.join("build-tools/36.0.0");
        std::fs::create_dir_all(&build_tools).unwrap();
        let binary = build_tools.join(format!("aapt2{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let task = Task::leaf(
            "compile",
            "app",
            vec![],
            vec![],
            TaskAction::RunTool {
                tool: AllowlistedTool::Aapt2,
                args: vec![build_tools.display().to_string(), "link".to_owned()],
                cwd: root.clone(),
            },
        );

        let baseline = fingerprint(&task, &ctx("cfg"));
        assert_eq!(baseline, fingerprint(&task, &ctx("cfg")));

        // A different installed binary (an SDK/toolchain upgrade) must
        // invalidate the task even though the action is byte-for-byte equal.
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n# v2\n").unwrap();
        assert_ne!(baseline, fingerprint(&task, &ctx("cfg")));
    }

    #[test]
    fn a_successful_action_that_misses_its_declared_output_fails() {
        let root = temp_dir("missing-output");
        let task = Task::leaf(
            "compile",
            "app",
            vec![],
            vec![root.join("out/Cls.class")],
            TaskAction::RunTool {
                tool: AllowlistedTool::Echo,
                args: vec!["x".to_owned()],
                cwd: root.clone(),
            },
        );
        let error = Executor::new([AllowlistedTool::Echo])
            .run_task(&task)
            .expect_err("missing output");
        assert!(error.contains("declared output"));
        assert!(error.contains("Cls.class"));
    }

    #[test]
    fn output_verification_refuses_to_record_a_missing_artifact() {
        let root = temp_dir("output-not-recorded");
        let mut graph = TaskGraph::new();
        graph
            .register(Task::leaf(
                "compile",
                "app",
                vec![],
                vec![root.join("out/Cls.class")],
                TaskAction::RunTool {
                    tool: AllowlistedTool::Echo,
                    args: vec!["x".to_owned()],
                    cwd: root.clone(),
                },
            ))
            .unwrap();

        let mut store = FingerprintStore::load(root.join("state.json")).unwrap();
        let first = Executor::new([AllowlistedTool::Echo])
            .execute(&graph, &ctx("cfg"), &mut store)
            .expect("schedules");
        let failure = first.failure.expect("missing output should fail");
        assert!(failure.error.contains("declared output"));

        // The missing artifact was not recorded up-to-date, so the task
        // still runs (and still fails) on the next build instead of being
        // skipped and leaving the absent output to hash as present.
        let second = Executor::new([AllowlistedTool::Echo])
            .execute(&graph, &ctx("cfg"), &mut store)
            .expect("schedules");
        assert!(second.failure.is_some());
    }

    #[test]
    fn a_copy_that_misses_its_declared_output_fails() {
        let root = temp_dir("copy-missing-output");
        std::fs::write(root.join("src.txt"), "data").unwrap();
        let task = Task::leaf(
            "stage",
            "app",
            vec![],
            vec![root.join("out.txt")],
            // The action succeeds but writes a different file than the
            // declared output — a path mismatch that must be a failure.
            TaskAction::Copy {
                from: root.join("src.txt"),
                to: root.join("produced.txt"),
            },
        );
        let error = Executor::new([])
            .run_task(&task)
            .expect_err("declared output missing");
        assert!(error.contains("declared output"));
        assert!(error.contains("out.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_on_path_finds_the_executable_binary() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("resolve-path");
        std::fs::write(root.join("javac"), "fake-tool").unwrap();
        std::fs::set_permissions(root.join("javac"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            resolve_on_path("javac", std::slice::from_ref(&root)),
            Some(root.join("javac"))
        );
    }

    #[test]
    fn resolve_on_path_returns_none_for_an_absent_tool() {
        let root = temp_dir("resolve-absent");
        assert_eq!(resolve_on_path("javac", std::slice::from_ref(&root)), None);
        assert_eq!(resolve_on_path("javac", &[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_on_path_continues_past_a_non_executable_to_a_later_executable() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("resolve-continue");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("javac"), "not-executable").unwrap();
        std::fs::write(second.join("javac"), "executable").unwrap();
        std::fs::set_permissions(first.join("javac"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(second.join("javac"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            resolve_on_path("javac", &[first, second]),
            Some(root.join("second/javac"))
        );
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

    #[test]
    fn split_cross_plugin_ref_parses_valid_format() {
        assert_eq!(
            split_cross_plugin_ref("ulite/fixture:stage"),
            Some(("ulite/fixture", "stage"))
        );
    }

    #[test]
    fn split_cross_plugin_ref_returns_none_for_bare_names() {
        assert_eq!(split_cross_plugin_ref("stage"), None);
        assert_eq!(split_cross_plugin_ref("compileKotlin"), None);
    }

    #[test]
    fn split_cross_plugin_ref_rejects_empty_parts() {
        assert_eq!(split_cross_plugin_ref(":stage"), None);
        assert_eq!(split_cross_plugin_ref("ulite/fixture:"), None);
    }

    #[test]
    fn strip_cross_plugin_refs_removes_cross_deps() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["stage".to_owned(), "ulite/fixture:stage".to_owned()],
                ..copy_task("consume", "x", "y")
            })
            .unwrap();
        graph.strip_cross_plugin_refs();
        let task = graph.get("app", "consume").unwrap();
        assert_eq!(task.depends_on, vec!["stage".to_owned()]);
    }

    #[test]
    fn strip_cross_plugin_refs_preserves_bare_deps() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["stage".to_owned()],
                ..copy_task("consume", "x", "y")
            })
            .unwrap();
        graph.strip_cross_plugin_refs();
        let task = graph.get("app", "consume").unwrap();
        assert_eq!(task.depends_on, vec!["stage".to_owned()]);
    }

    #[test]
    fn resolve_cross_plugin_dep_orders_tasks() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["ulite/fixture:stage".to_owned()],
                ..copy_task("consume", "app", "y")
            })
            .unwrap();
        graph.register(copy_task("stage", "a", "b")).unwrap();

        // Both tasks live under the same module label here, mirroring a
        // single-plugin-per-module world: the provider of any plugin's
        // reference is that module itself.
        graph
            .resolve_cross_plugin_deps(&|_consumer, _plugin, task| Some(format!("app::{task}")))
            .unwrap();

        let consume = graph.get("app", "consume").unwrap();
        assert_eq!(consume.depends_on, vec!["app::stage".to_owned()]);

        let waves = graph.waves().unwrap();
        assert_eq!(waves.len(), 2, "stage in wave 0, consume in wave 1");
    }

    #[test]
    fn resolve_rejects_undeclared_plugin() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["ulite/unknown:task".to_owned()],
                ..copy_task("a", "x", "y")
            })
            .unwrap();
        let error = graph
            .resolve_cross_plugin_deps(&|_, _, _| None)
            .expect_err("unknown plugin");
        assert!(error.to_string().contains("ulite/unknown:task"));
    }

    #[test]
    fn resolve_rejects_unknown_task_within_known_plugin() {
        let mut graph = TaskGraph::new();
        graph
            .register(Task {
                depends_on: vec!["ulite/fixture:ghost".to_owned()],
                ..copy_task("a", "x", "y")
            })
            .unwrap();
        let error = graph
            .resolve_cross_plugin_deps(&|consumer_module, _plugin, task| {
                // The plugin is known but only registers other tasks.
                if task == "stage" {
                    Some(format!("{consumer_module}::stage"))
                } else {
                    None
                }
            })
            .expect_err("unknown task");
        assert!(error.to_string().contains("ulite/fixture:ghost"));
    }
}
