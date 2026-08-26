//! A test plugin that registers build tasks through the `configure` entry
//! point.
//!
//! It implements the guest side of the plugin ABI (ARCHITECTURE.md §3.2):
//! given the module's configuration as JSON — `{"source": <file>,
//! "output": <file>}` — it registers a `stage` task that copies the source
//! to the output and an independent `announce` task that runs the
//! allowlisted `echo` tool. When the configuration also carries the
//! host-resolved `classpath` object and a `classpathOutput` path, it
//! registers a `copy-classpath` task that copies the first compile jar
//! there, proving a plugin can consume the jars the host resolved for its
//! `deps {}` block. A `sourceSetClasspath` config key (an object naming a
//! source set and an output path) registers a task copying the first
//! compile jar of that source set's resolved `classpathSourceSets` entry.
//! A `probeTool` config key additionally registers a
//! no-op `run-tool` task with the named tool, so the host tests can drive
//! the manifest-declared-tools gate (ARCHITECTURE.md §3.5). A
//! `probeAndroidSdk` config key makes configure assert that the module's
//! `android.sdkDir` (or the injected `androidSdkDir`) is readable from the
//! guest, proving the host preopened it. A `buildConfigProbe` config key
//! reads `android.buildConfigField` list triples and writes the parsed
//! fields to a file, proving the host passes buildConfigField entries to the
//! plugin. The `uliab`
//! integration tests build this crate for `wasm32-wasip2` and drive it
//! through [`uliab::host::PluginHost`] to prove configure -> task graph ->
//! execute end to end.
#![allow(unsafe_code)]

wit_bindgen::generate!({
    path: "../ulb-plugin-sdk/plugin.wit",
    world: "plugin",
});

use ulite::ulb::task_registrar::{
    self, Action, AllowlistedTool, CopyArgs, RunToolArgs, Task, WriteFileArgs,
};

use ulb_plugin_sdk::UlbConfig;

/// Typed config schema for the fixture plugin. Only the fields exercised
/// by integration tests are declared here; the actual `configure` fn
/// still reads from raw JSON so test-only keys (e.g. `infiniteLoop`)
/// remain ad-hoc.
#[derive(UlbConfig, serde::Deserialize)]
pub struct FixtureConfig {
    /// Source file path to copy from.
    pub source: String,
    /// Destination file path to copy to.
    pub output: String,
    /// Host-resolved classpath object (optional).
    #[ulb(description = "Resolved classpath with compile/runtime buckets")]
    #[serde(default)]
    pub classpath: Option<serde_json::Value>,
    /// Output path for the first compile jar (optional).
    #[serde(default)]
    pub classpath_output: Option<String>,
    /// Source set classpath specification (optional).
    #[serde(default)]
    pub source_set_classpath: Option<serde_json::Value>,
    /// Tool name to probe via a no-op run-tool task (optional).
    #[serde(default)]
    pub probe_tool: Option<String>,
    /// Whether to assert android.sdkDir readability (optional).
    #[serde(default)]
    pub probe_android_sdk: Option<bool>,
    /// Whether to exercise buildConfigField parsing (optional).
    #[serde(default)]
    pub build_config_probe: Option<bool>,
}

ulb_plugin_sdk::embed_schema!(FixtureConfig);

/// The fixture plugin: a copy task plus an echo task per configuration.
struct Fixture;

/// Maps a tool name from the module configuration onto the WIT
/// allowlisted-tool enum. Covers the tools a fixture task plausibly runs;
/// the build-tool binaries (`aapt2`, `apksigner`) are outside what these
/// tests exercise and are rejected like any unknown name.
fn parse_tool(name: &str) -> Result<AllowlistedTool, String> {
    Ok(match name {
        "echo" => AllowlistedTool::Echo,
        "cp" => AllowlistedTool::Cp,
        "cat" => AllowlistedTool::Cat,
        "mkdir" => AllowlistedTool::Mkdir,
        "javac" => AllowlistedTool::Javac,
        "kotlinc" => AllowlistedTool::Kotlinc,
        "jar" => AllowlistedTool::Jar,
        "java" => AllowlistedTool::Java,
        other => return Err(format!("unknown tool '{other}'")),
    })
}

impl exports::ulite::ulb::ulb_plugin::Guest for Fixture {
    fn manifest() -> exports::ulite::ulb::ulb_plugin::PluginManifest {
        exports::ulite::ulb::ulb_plugin::PluginManifest {
            name: "ulite/fixture".to_owned(),
            version: "0.1.0".to_owned(),
            abi_version: ulb_plugin_sdk::ABI_VERSION.to_owned(),
            tools: vec!["echo".to_owned(), "mkdir".to_owned()],
            dependencies: Vec::new(),
        }
    }

    fn configure(module_config: String) -> Result<(), String> {
        let config: serde_json::Value = serde_json::from_str(&module_config)
            .map_err(|error| format!("invalid module config JSON: {error}"))?;
        // A `infiniteLoop` config key (true) makes configure loop forever,
        // so the host tests can prove fuel metering terminates a runaway
        // plugin instead of hanging the build. A `memoryHog` config key
        // (true) makes it allocate 512 MB of linear memory, past the
        // host's default memory cap, so the tests can prove the
        // resource limiter traps a runaway allocation.
        if config
            .get("infiniteLoop")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let mut n = 0u64;
            loop {
                n = n.wrapping_add(1);
                std::hint::black_box(n);
            }
        }
        if config.get("memoryHog").and_then(serde_json::Value::as_bool) == Some(true) {
            let hog: Vec<u8> = std::hint::black_box(vec![0u8; 512 * 1024 * 1024]);
            std::hint::black_box(&hog);
        }
        let source = config
            .get("source")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "module config is missing 'source'".to_owned())?
            .to_owned();
        let output = config
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "module config is missing 'output'".to_owned())?
            .to_owned();

        task_registrar::register_task(&Task {
            name: "stage".to_owned(),
            inputs: vec![source.clone()],
            outputs: vec![output.clone()],
            depends_on: Vec::new(),
            action: Action::CopyFile(CopyArgs {
                source: source.clone(),
                destination: output,
            }),
        })?;

        task_registrar::register_task(&Task {
            name: "announce".to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            depends_on: Vec::new(),
            action: Action::RunTool(RunToolArgs {
                tool: AllowlistedTool::Echo,
                args: vec!["staged".to_owned(), source],
                cwd: ".".to_owned(),
            }),
        })?;

        let compile_jar = config
            .get("classpath")
            .and_then(|classpath| classpath.get("compile"))
            .and_then(serde_json::Value::as_array)
            .and_then(|jars| jars.first())
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let classpath_output = config
            .get("classpathOutput")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let (Some(compile_jar), Some(classpath_output)) = (compile_jar, classpath_output) {
            task_registrar::register_task(&Task {
                name: "copy-classpath".to_owned(),
                inputs: Vec::new(),
                outputs: vec![classpath_output.clone()],
                depends_on: Vec::new(),
                action: Action::CopyFile(CopyArgs {
                    source: compile_jar,
                    destination: classpath_output,
                }),
            })?;
        }

        // A `sourceSetClasspath` config key (an object with `name` and
        // `output`, plus an optional zero-based `index`) registers a task
        // that copies one compile jar of that source set's classpath,
        // proving the host-resolved `classpathSourceSets` map reaches the
        // plugin. The default index 0 covers the common case; tests use a
        // deeper index to observe jars beyond the first entry.
        if let Some(spec) = config.get("sourceSetClasspath") {
            let name = spec
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "sourceSetClasspath is missing 'name'".to_owned())?;
            let output = spec
                .get("output")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "sourceSetClasspath is missing 'output'".to_owned())?
                .to_owned();
            let index = spec
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize;
            let compile_jar = config
                .get("classpathSourceSets")
                .and_then(|sets| sets.get(name))
                .and_then(|classpath| classpath.get("compile"))
                .and_then(serde_json::Value::as_array)
                .and_then(|jars| jars.get(index))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("no compile jar at index {index} for source set '{name}'"))?
                .to_owned();
            task_registrar::register_task(&Task {
                name: "copy-source-set-classpath".to_owned(),
                inputs: Vec::new(),
                outputs: vec![output.clone()],
                depends_on: Vec::new(),
                action: Action::CopyFile(CopyArgs {
                    source: compile_jar,
                    destination: output,
                }),
            })?;
        }

        // A `probeTool` config key registers a no-op run-tool task with that
        // tool, letting the host tests exercise the manifest-declared-tools
        // gate (§3.5): a tool the manifest does not declare is refused.
        if let Some(tool_name) = config.get("probeTool").and_then(serde_json::Value::as_str) {
            let tool = parse_tool(tool_name)?;
            task_registrar::register_task(&Task {
                name: "probe".to_owned(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                depends_on: Vec::new(),
                action: Action::RunTool(RunToolArgs {
                    tool,
                    args: Vec::new(),
                    cwd: ".".to_owned(),
                }),
            })?;
        }

        // A `variantProbe` config key (true) mirrors the plugin family's
        // documented variant naming rule (buildTypes {} keys — or the
        // default debug/release pair — crossed with productFlavors {}
        // flavors, PascalCase-joined) and registers one no-op `probe<V>`
        // echo task per computed variant. Host tests use it to prove
        // variant selection restricts which tasks get registered at all.
        if config
            .get("variantProbe")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let pascal = |name: &str| -> String {
                name.split('_')
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        let mut chars = part.chars();
                        match chars.next() {
                            None => String::new(),
                            Some(first) => {
                                first.to_uppercase().collect::<String>()
                                    + &chars.collect::<String>()
                            }
                        }
                    })
                    .collect()
            };
            let build_types: Vec<String> = match config.get("buildTypes") {
                Some(bt) => bt
                    .as_object()
                    .ok_or_else(|| "'buildTypes' must be a block".to_owned())?
                    .keys()
                    .cloned()
                    .collect(),
                None => vec!["debug".to_owned(), "release".to_owned()],
            };
            let flavors: Vec<String> = match config.get("productFlavors") {
                Some(pf) => pf
                    .as_object()
                    .ok_or_else(|| "'productFlavors' must be a block".to_owned())?
                    .keys()
                    .filter(|key| key.as_str() != "dimension")
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            if flavors.is_empty() {
                for bt in &build_types {
                    let name = pascal(bt);
                    task_registrar::register_task(&Task {
                        name: format!("probe{name}"),
                        inputs: Vec::new(),
                        outputs: Vec::new(),
                        depends_on: Vec::new(),
                        action: Action::RunTool(RunToolArgs {
                            tool: AllowlistedTool::Echo,
                            args: vec!["probe".to_owned(), name],
                            cwd: ".".to_owned(),
                        }),
                    })?;
                }
            } else {
                for bt in &build_types {
                    for flavor in &flavors {
                        let name = format!("{}{}", pascal(bt), pascal(flavor));
                        task_registrar::register_task(&Task {
                            name: format!("probe{name}"),
                            inputs: Vec::new(),
                            outputs: Vec::new(),
                            depends_on: Vec::new(),
                            action: Action::RunTool(RunToolArgs {
                                tool: AllowlistedTool::Echo,
                                args: vec!["probe".to_owned(), name],
                                cwd: ".".to_owned(),
                            }),
                        })?;
                    }
                }
            }
        }

        // A `mkdirProbe` config key (a directory name) registers a run-tool
        // task creating `<projectDir>/<name>`. The host injects `projectDir`
        // into every plugin configuration, so the created directory proves
        // the key reached the plugin with the real project path.
        if let Some(name) = config.get("mkdirProbe").and_then(serde_json::Value::as_str) {
            let project_dir = config
                .get("projectDir")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "module config is missing 'projectDir'".to_owned())?;
            task_registrar::register_task(&Task {
                name: "mkdir-probe".to_owned(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                depends_on: Vec::new(),
                action: Action::RunTool(RunToolArgs {
                    tool: AllowlistedTool::Mkdir,
                    args: vec!["-p".to_owned(), format!("{project_dir}/{name}")],
                    cwd: ".".to_owned(),
                }),
            })?;
        }

        // A `writeProbe` config key (`{"path": <file>, "contents": <text>}`)
        // registers a write-file task producing that file, proving a plugin
        // can generate a file with fixed contents (the jvm plugin's
        // generated test runner does exactly this).
        if let Some(write) = config.get("writeProbe") {
            let path = write
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "writeProbe is missing a 'path' string".to_owned())?;
            let contents = write
                .get("contents")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "writeProbe is missing a 'contents' string".to_owned())?;
            task_registrar::register_task(&Task {
                name: "write-probe".to_owned(),
                inputs: Vec::new(),
                outputs: vec![path.to_owned()],
                depends_on: Vec::new(),
                action: Action::WriteFile(WriteFileArgs {
                    path: path.to_owned(),
                    contents: contents.to_owned(),
                }),
            })?;
        }

        // A `probeAndroidSdk` config key (true) makes configure assert that
        // an Android SDK root is readable from the guest: the module's own
        // `android.sdkDir` when the block declares one, else the injected
        // `androidSdkDir`. The host preopens those paths read-only into the
        // guest filesystem, so a plugin that discovers SDK components can
        // inspect them; this probe fails configure when the path is not
        // actually reachable — exactly what the android plugin does, minus
        // the platform-jar and build-tools logic. It lets the driver tests
        // prove the preopen without shipping the real plugin.
        if config
            .get("probeAndroidSdk")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            let sdk_dir = config
                .get("android")
                .and_then(|block| block.get("sdkDir"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    config
                        .get("androidSdkDir")
                        .and_then(serde_json::Value::as_str)
                })
                .ok_or_else(|| {
                    "probeAndroidSdk needs 'android.sdkDir' or the injected 'androidSdkDir'"
                        .to_owned()
                })?;
            // Resolve a relative block path against the injected
            // `projectDir`, exactly as the android plugin resolves its
            // `sdkDir` — the preopened guest path is the absolute one.
            let sdk_dir = if std::path::Path::new(sdk_dir).is_absolute() {
                sdk_dir.to_owned()
            } else {
                let project_dir = config
                    .get("projectDir")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "module config is missing 'projectDir'".to_owned())?;
                format!("{project_dir}/{sdk_dir}")
            };
            std::fs::metadata(&sdk_dir).map_err(|error| {
                format!("android SDK root '{sdk_dir}' is not readable from the plugin: {error}")
            })?;
        }

        // A `buildConfigProbe` config key (a file path) makes configure
        // read the module's `android.buildConfigField` entries (list
        // triples), format them as "TYPE NAME = INITIALIZER;" lines, and
        // write the result to the given path. The host tests use this to
        // prove buildConfigField triples reach the plugin and the
        // three-element list form is parseable from the evaluated model.
        if let Some(path) = config
            .get("buildConfigProbe")
            .and_then(serde_json::Value::as_str)
        {
            let fields = config
                .get("android")
                .and_then(|block| block.get("buildConfigField"))
                .and_then(serde_json::Value::as_array);
            let mut body = String::new();
            if let Some(arr) = fields {
                let mut i = 0;
                while i < arr.len() {
                    if let serde_json::Value::Array(sub) = &arr[i]
                        && sub.len() == 3
                    {
                        let ty = sub[0].as_str().unwrap_or("?");
                        let name = sub[1].as_str().unwrap_or("?");
                        let init = sub[2].as_str().unwrap_or("?");
                        body.push_str(&format!("{ty} {name} = {init};\n"));
                        i += 1;
                        continue;
                    }
                    if i + 2 < arr.len()
                        && let (Some(a), Some(b), Some(c)) =
                            (arr[i].as_str(), arr[i + 1].as_str(), arr[i + 2].as_str())
                    {
                        body.push_str(&format!("{a} {b} = {c};\n"));
                        i += 3;
                        continue;
                    }
                    i += 1;
                }
            }
            task_registrar::register_task(&Task {
                name: "buildconfig-probe".to_owned(),
                inputs: Vec::new(),
                outputs: vec![path.to_owned()],
                depends_on: Vec::new(),
                action: Action::WriteFile(WriteFileArgs {
                    path: path.to_owned(),
                    contents: body,
                }),
            })?;
        }

        Ok(())
    }

    fn run(input: String) -> String {
        input
    }
}

#[cfg(target_arch = "wasm32")]
export!(Fixture);
