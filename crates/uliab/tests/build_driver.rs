//! End-to-end tests of the core build driver (ARCHITECTURE.md §9): a real
//! `.ulb` project whose `libs.ulb` declares the fixture plugin, resolved
//! through a local registry index, evaluated and executed by
//! [`uliab::driver::build_project`].
//!
//! The fixture component is built here with a nested `cargo build` for
//! `wasm32-wasip2`, exactly as in `configure_execute.rs`, so the driver
//! test can never drift from the current `plugin.wit`.

use std::path::PathBuf;
use std::process::Command;

use uliab::driver::{BuildOptions, build_project};
use uliab::maven::MavenRepo;
use uliab::registry::RegistrySource;

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

/// A temp directory that doubles as both a test project and its registry:
/// `project/` holds the `.ulb` sources, `project/index.json` is a one-entry
/// registry index pointing at a built fixture, and the plugin cache is kept
/// inside the project so tests never touch the user's real cache.
struct TestProject {
    dir: PathBuf,
}

impl TestProject {
    fn new(label: &str, fixture: &std::path::Path) -> Self {
        let dir = std::env::temp_dir().join(format!("uliab-driver-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let index = serde_json::json!({
            "schema_version": 1,
            "plugins": {
                "ulite/fixture": {
                    "versions": {
                        "0.1.0": {
                            "abi": { "min": "0.4", "max": "0.5" },
                            "artifact_url": fixture.display().to_string(),
                        }
                    }
                }
            }
        });
        std::fs::write(dir.join("index.json"), index.to_string()).expect("write registry index");
        let project = Self { dir };
        project.write("in.txt", "hello from the driver");
        project
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.dir.join(name), contents).expect("write project file");
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(name)).expect("read project file")
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            registry: Some(RegistrySource::File(self.dir.join("index.json"))),
            cache_dir: Some(self.dir.join(".cache")),
            repos: None,
            android_sdk: None,
        }
    }
}

#[test]
fn driver_builds_and_executes_a_project_incrementally() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("incremental", &fixture);
    let source = project.dir.join("in.txt");
    let output = project.dir.join("out.txt");
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\n",
            source.display().to_string(),
            output.display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let options = project.options();
    let first = build_project(&project.dir, &options).expect("first build");
    assert_eq!((first.ran, first.up_to_date, first.skipped), (2, 0, 0));
    assert_eq!(project.read("out.txt"), b"hello from the driver");
    assert!(project.dir.join(".uliab/state.json").is_file());

    // Unchanged sources: everything is up-to-date.
    let second = build_project(&project.dir, &options).expect("second build");
    assert_eq!((second.ran, second.up_to_date), (0, 2));

    // Touching the source file reruns only the task that reads it: `stage`
    // copies in.txt, while `announce` echoes a string and declares no
    // inputs, so its fingerprint is unchanged.
    project.write("in.txt", "changed input");
    let third = build_project(&project.dir, &options).expect("third build");
    assert_eq!((third.ran, third.up_to_date), (1, 1));
    assert_eq!(project.read("out.txt"), b"changed input");
}

#[test]
fn driver_surfaces_a_plugin_that_rejects_the_configuration() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("rejected", &fixture);
    // The fixture requires `source` and `output`; a model without them is
    // rejected in-band by the plugin's configure entry.
    project.write("build.ulb", "someOtherKey = \"ignored\"\n");
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let error = build_project(&project.dir, &project.options()).expect_err("plugin rejects");
    assert!(error.contains("rejected the configuration"), "{error}");
    assert!(error.contains("missing 'source'"), "{error}");
}

#[test]
fn driver_reports_a_project_that_declares_no_plugins() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("noplugins", &fixture);
    project.write("build.ulb", "source = \"in.txt\"\n");
    project.write(
        "libs.ulb",
        "appcompat = \"androidx.appcompat:appcompat:1.7.0\"\n",
    );

    let error = build_project(&project.dir, &project.options()).expect_err("no plugins");
    assert!(error.contains("declares no plugins"), "{error}");
}

#[test]
fn deps_are_resolved_and_reach_the_plugin() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("deps", &fixture);

    // A local Maven repository carrying one jar artifact.
    let repo = project.dir.join("repo");
    let artifact_dir = repo.join("com/example/libs/1.0");
    std::fs::create_dir_all(&artifact_dir).expect("repo dir");
    std::fs::write(
        artifact_dir.join("libs-1.0.pom"),
        "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion>\
         <groupId>com.example</groupId><artifactId>libs</artifactId><version>1.0</version>\
         </project>",
    )
    .expect("write pom");
    std::fs::write(artifact_dir.join("libs-1.0.jar"), b"jar contents").expect("write jar");

    let source = project.dir.join("in.txt");
    let output = project.dir.join("out.txt");
    let classpath_output = project.dir.join("copied.jar");
    project.write("in.txt", "hello");
    project.write(
        "build.ulb",
        &format!(
            "deps {{\n  implementation \"com.example:libs:1.0\"\n}}\n\
             source = {:?}\noutput = {:?}\nclasspathOutput = {:?}\n",
            source.display().to_string(),
            output.display().to_string(),
            classpath_output.display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.repos = Some(vec![MavenRepo::Custom(repo.display().to_string())]);
    let first = build_project(&project.dir, &options).expect("first build");
    assert_eq!((first.ran, first.up_to_date), (3, 0));
    assert_eq!(project.read("copied.jar"), b"jar contents");

    // Unchanged sources: the classpath-copy task is up-to-date like the rest.
    let second = build_project(&project.dir, &options).expect("second build");
    assert_eq!((second.ran, second.up_to_date), (0, 3));
}

#[test]
fn project_dir_is_handed_to_plugins() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("project-dir", &fixture);
    let source = project.dir.join("in.txt");
    let output = project.dir.join("out.txt");
    project.write("in.txt", "hello");
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\nmkdirProbe = \"from-plugin\"\n",
            source.display().to_string(),
            output.display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let result = build_project(&project.dir, &project.options()).expect("build");
    assert_eq!((result.ran, result.up_to_date), (3, 0));
    assert!(
        project.dir.join("from-plugin").is_dir(),
        "the plugin created <projectDir>/from-plugin, so projectDir reached it"
    );
}

#[test]
fn a_module_sdk_dir_is_preopened_and_readable_from_the_plugin() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("module-sdk", &fixture);
    // A fake SDK tree living next to the project. The module declares it
    // with a relative `android.sdkDir`, which the host must resolve against
    // the project directory before preopening; the plugin then discovers
    // the SDK at that path during configure (probeAndroidSdk) and would see
    // NOTCAPABLE if the host had skipped it.
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\nandroid {{\n  sdkDir = \"fake-sdk\"\n}}\nprobeAndroidSdk = true\n",
            project.dir.join("in.txt").display().to_string(),
            project.dir.join("out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );
    std::fs::create_dir_all(project.dir.join("fake-sdk/platforms/android-36")).expect("fake sdk");

    let result = build_project(&project.dir, &project.options()).expect("build");
    assert_eq!((result.ran, result.up_to_date), (2, 0));
}

#[test]
fn an_injected_sdk_root_is_preopened_and_readable_from_the_plugin() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("injected-sdk", &fixture);
    let sdk = project.dir.join("fake-sdk");
    std::fs::create_dir_all(sdk.join("platforms/android-36")).expect("fake sdk");
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\nprobeAndroidSdk = true\n",
            project.dir.join("in.txt").display().to_string(),
            project.dir.join("out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.android_sdk = Some(sdk);
    let result = build_project(&project.dir, &options).expect("build");
    assert_eq!((result.ran, result.up_to_date), (2, 0));
}

#[test]
fn an_explicit_sdk_override_that_does_not_exist_fails_the_build() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("missing-sdk", &fixture);
    let missing = project.dir.join("no-such-sdk");
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\n",
            project.dir.join("in.txt").display().to_string(),
            project.dir.join("out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.android_sdk = Some(missing.clone());
    let error = build_project(&project.dir, &options).expect_err("missing SDK");
    assert!(error.contains(&missing.display().to_string()), "{error}");
    assert!(error.contains("must name an existing directory"), "{error}");
}
