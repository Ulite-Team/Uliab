//! Schema round-trip test: build the fixture plugin for wasm32-wasip2,
//! read the compiled `.wasm`, and verify `extract_schema` returns the
//! expected typed config surface embedded by `#[derive(UlbConfig)]`.

use std::path::PathBuf;
use std::process::Command;

use uliab::schema::extract_schema;

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

#[test]
fn fixture_plugin_embeds_config_schema() {
    let wasm_path = build_fixture("ulb-plugin-fixture");
    let wasm = std::fs::read(&wasm_path).expect("read fixture wasm");

    let schema = extract_schema(&wasm).expect("fixture should embed a schema custom section");

    assert_eq!(schema.name, "FixtureConfig");
    assert!(
        !schema.properties.is_empty(),
        "schema should declare config properties"
    );

    let source = schema
        .properties
        .iter()
        .find(|p| p.name == "source")
        .expect("schema should have a 'source' field");
    assert_eq!(source.type_name, "string");
    assert!(source.required, "source field should be required");
    assert!(
        !source.description.is_empty(),
        "source field should have a description from its doc comment"
    );

    let output = schema
        .properties
        .iter()
        .find(|p| p.name == "output")
        .expect("schema should have an 'output' field");
    assert_eq!(output.type_name, "string");
    assert!(output.required, "output field should be required");

    let classpath = schema
        .properties
        .iter()
        .find(|p| p.name == "classpath")
        .expect("schema should have a 'classpath' field");
    assert_eq!(classpath.type_name, "object");
    assert!(!classpath.required, "classpath field should be optional");
    assert!(
        classpath.description.contains("Resolved classpath"),
        "classpath description should come from the #[ulb(description)] attribute, got: {}",
        classpath.description
    );

    let probe_tool = schema
        .properties
        .iter()
        .find(|p| p.name == "probe_tool")
        .expect("schema should have a 'probe_tool' field");
    assert_eq!(probe_tool.type_name, "string");
    assert!(!probe_tool.required, "probe_tool should be optional");
}
