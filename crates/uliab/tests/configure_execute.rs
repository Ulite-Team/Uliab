//! End-to-end tests of the plugin ABI: build the fixture components for
//! `wasm32-wasip2`, drive them through [`uliab::host::PluginHost`], and
//! prove the configure -> task graph -> execute path works against real
//! wasm.
//!
//! The fixtures live in the workspace (`crates/ulb-plugin-fixture`,
//! `crates/ulb-plugin-legacy-fixture`) and are built here with a nested
//! `cargo build` so they can never drift from the current `plugin.wit`
//! the host itself compiles against.

use std::path::PathBuf;
use std::process::Command;

use uliab::host::{HostError, PluginHost};
use uliab::task::{Executor, FingerprintContext, FingerprintStore};

/// The workspace root, i.e. the directory two levels above this crate.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/uliab has a parent")
        .parent()
        .expect("workspace root has a parent")
        .to_path_buf()
}

/// The build output directory, honouring `CARGO_TARGET_DIR` when set.
fn target_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    workspace_root().join("target")
}

/// Builds `package` for `wasm32-wasip2` and returns its component path.
fn build_fixture(package: &str) -> PathBuf {
    let status = Command::new("cargo")
        .args(["build", "-p", package, "--target", "wasm32-wasip2"])
        .current_dir(workspace_root())
        .status()
        .expect("nested cargo build for the fixture");
    assert!(status.success(), "building {package} failed");
    let artifact = target_dir()
        .join("wasm32-wasip2/debug")
        .join(format!("{}.wasm", package.replace('-', "_")));
    assert!(
        artifact.is_file(),
        "fixture artifact missing: {}",
        artifact.display()
    );
    artifact
}

/// A temp workspace for one test: source file plus a state-store slot.
fn temp_workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("uliab-integ-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn configure_registers_tasks_that_execute_incrementally() {
    let plugin = build_fixture("ulb-plugin-fixture");
    let workdir = temp_workdir("configure-execute");
    let source = workdir.join("in.txt");
    std::fs::write(&source, "hello from the plugin").unwrap();
    let config_json = format!(
        r#"{{"source": {}, "output": {}}}"#,
        serde_json::to_string(&source.display().to_string()).unwrap(),
        serde_json::to_string(&workdir.join("out.txt").display().to_string()).unwrap(),
    );

    let host = PluginHost::new().expect("host engine");
    let manifest = host
        .manifest_of_bytes(&std::fs::read(&plugin).unwrap())
        .unwrap();
    assert_eq!(manifest.name, "ulite/fixture");
    assert_eq!(
        manifest.abi_version,
        ulb_plugin_sdk::ABI_VERSION,
        "fixture reports the host ABI"
    );

    let graph = host
        .configure(&plugin, "app", &config_json)
        .expect("plugin configures");
    assert_eq!(graph.len(), 2);
    let stage = graph.get("app", "stage").expect("stage task");
    assert_eq!(
        stage.action,
        uliab::task::TaskAction::Copy {
            from: source.clone(),
            to: workdir.join("out.txt"),
        }
    );
    assert_eq!(
        graph.get("app", "announce").expect("announce task").module,
        "app"
    );

    let ctx = FingerprintContext {
        plugin_version: "0.1.0".to_owned(),
        config_hash: "cfg-v1".to_owned(),
    };
    let mut store = FingerprintStore::load(workdir.join("state.json")).unwrap();
    let executor = Executor::new([uliab::task::AllowlistedTool::Echo]);

    let first = executor
        .execute(&graph, &ctx, &mut store)
        .expect("schedules");
    assert_eq!((first.ran, first.up_to_date), (2, 0));
    assert_eq!(
        std::fs::read(workdir.join("out.txt")).unwrap(),
        b"hello from the plugin"
    );

    let second = executor
        .execute(&graph, &ctx, &mut store)
        .expect("schedules");
    assert_eq!((second.ran, second.up_to_date), (0, 2));

    store.save().unwrap();
    let mut reloaded = FingerprintStore::load(workdir.join("state.json")).unwrap();
    let third = executor
        .execute(&graph, &ctx, &mut reloaded)
        .expect("schedules");
    assert_eq!((third.ran, third.up_to_date), (0, 2));
}

#[test]
fn configure_reports_plugin_rejection() {
    let plugin = build_fixture("ulb-plugin-fixture");
    let host = PluginHost::new().expect("host engine");
    let error = host
        .configure(&plugin, "app", "not json")
        .expect_err("plugin rejects bad config");
    assert!(matches!(error, HostError::Call(_)));
    assert!(error.to_string().contains("invalid module config JSON"));
}

#[test]
fn write_file_task_generates_and_reruns_on_content_change() {
    let plugin = build_fixture("ulb-plugin-fixture");
    let workdir = temp_workdir("write-file");
    let generated = workdir.join("generated/Runner.java");
    std::fs::write(workdir.join("in.txt"), "input").unwrap();
    let config_json = format!(
        r#"{{"source": {}, "output": {}, "writeProbe": {{"path": {}, "contents": "v1"}}}}"#,
        serde_json::to_string(&workdir.join("in.txt").display().to_string()).unwrap(),
        serde_json::to_string(&workdir.join("out.txt").display().to_string()).unwrap(),
        serde_json::to_string(&generated.display().to_string()).unwrap(),
    );

    let host = PluginHost::new().expect("host engine");
    let graph = host
        .configure(&plugin, "app", &config_json)
        .expect("plugin configures");
    assert_eq!(
        graph
            .get("app", "write-probe")
            .expect("write-probe task")
            .action,
        uliab::task::TaskAction::WriteFile {
            to: generated.clone(),
            contents: "v1".to_owned(),
        }
    );

    let ctx = FingerprintContext {
        plugin_version: "0.1.0".to_owned(),
        config_hash: "cfg-write".to_owned(),
    };
    let mut store = FingerprintStore::load(workdir.join("state.json")).unwrap();
    let executor = Executor::new([uliab::task::AllowlistedTool::Echo]);

    let first = executor
        .execute(&graph, &ctx, &mut store)
        .expect("schedules");
    assert_eq!(first.ran, 3);
    assert_eq!(std::fs::read(&generated).unwrap(), b"v1");

    let second = executor
        .execute(&graph, &ctx, &mut store)
        .expect("schedules");
    assert_eq!((second.ran, second.up_to_date), (0, 3));

    // A second configure with different write contents produces a different
    // write action. Under the same config hash the write task's own
    // fingerprint (the rendered action includes the contents) is what forces
    // it to rerun, while the copy and echo tasks stay up-to-date.
    let changed_config_json = config_json.replace("\"contents\": \"v1\"", "\"contents\": \"v2\"");
    let changed_graph = host
        .configure(&plugin, "app", &changed_config_json)
        .expect("plugin reconfigures");
    let third = executor
        .execute(&changed_graph, &ctx, &mut store)
        .expect("schedules");
    assert_eq!((third.ran, third.up_to_date), (1, 2));
    assert_eq!(std::fs::read(&generated).unwrap(), b"v2");
}

#[test]
fn legacy_component_still_instantiates_and_runs() {
    let plugin = build_fixture("ulb-plugin-legacy-fixture");
    let host = PluginHost::new().expect("host engine");

    let output = host.run(&plugin, "echo me").expect("legacy run");
    assert_eq!(output, "echo me");

    // The legacy component never grew a configure entry, so requesting one
    // fails at instantiation against the full world rather than pretending
    // to succeed.
    let error = host
        .configure(&plugin, "app", "{}")
        .expect_err("legacy has no configure");
    assert!(matches!(error, HostError::Load(_)));
    assert!(error.to_string().contains("configure"));
}
