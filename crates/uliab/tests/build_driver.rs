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
                            "abi": { "min": "0.4", "max": "0.7" },
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
            variants: None,
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
fn source_set_deps_are_resolved_and_reach_the_plugin() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("source-set-deps", &fixture);

    // A local Maven repository carrying one jar artifact for the source set.
    let repo = project.dir.join("repo");
    let artifact_dir = repo.join("com/example/common/1.0");
    std::fs::create_dir_all(&artifact_dir).expect("repo dir");
    std::fs::write(
        artifact_dir.join("common-1.0.pom"),
        "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion>\
         <groupId>com.example</groupId><artifactId>common</artifactId><version>1.0</version>\
         </project>",
    )
    .expect("write pom");
    std::fs::write(artifact_dir.join("common-1.0.jar"), b"common jar").expect("write jar");

    let source = project.dir.join("in.txt");
    let output = project.dir.join("out.txt");
    let copied = project.dir.join("copied-common.jar");
    project.write("in.txt", "hello");
    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\n\
             commonMain.deps {{\n  implementation \"com.example:common:1.0\"\n}}\n\
             sourceSetClasspath {{\n  name = \"commonMain\"\n  output = {:?}\n}}\n",
            source.display().to_string(),
            output.display().to_string(),
            copied.display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.repos = Some(vec![MavenRepo::Custom(repo.display().to_string())]);
    let first = build_project(&project.dir, &options).expect("first build");
    // stage, announce, and the source-set classpath copy all ran.
    assert_eq!((first.ran, first.up_to_date), (3, 0));
    assert_eq!(project.read("copied-common.jar"), b"common jar");
    // The source-set classpath is part of the config hash: an unchanged
    // build stays fully up-to-date, like the module-level classpath copy.
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

/// Writes a one-artifact Maven repository layout under `repo/` and returns
/// the artifact directory.
fn write_artifact(project: &TestProject, group: &str, name: &str, jar_bytes: &[u8]) -> PathBuf {
    let artifact_dir = project.dir.join("repo").join(group).join(name).join("1.0");
    std::fs::create_dir_all(&artifact_dir).expect("repo dir");
    std::fs::write(
        artifact_dir.join(format!("{name}-1.0.pom")),
        format!(
            "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion>\
             <groupId>{group}</groupId><artifactId>{name}</artifactId><version>1.0</version>\
             </project>"
        ),
    )
    .expect("write pom");
    std::fs::write(artifact_dir.join(format!("{name}-1.0.jar")), jar_bytes).expect("write jar");
    artifact_dir
}

/// A consumer module listed *before* its dependency in settings.ulb gets
/// the dependency's output and api-scoped jars on its source-set compile
/// classpath: `commonMain.deps { api project(":lib") }` resolves even
/// though `app` is evaluated first, proving declaration order plays no
/// role in cross-module resolution.
#[test]
fn source_set_project_refs_propagate_api_classpaths_regardless_of_declaration_order() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("source-set-project-refs", &fixture);

    let bridge = write_artifact(&project, "com/example", "bridge", b"bridge jar");
    write_artifact(&project, "com/example", "shared", b"shared jar");

    project.write(
        "settings.ulb",
        "project \"RefOrder\"\nmodule \"app\"\nmodule \"lib\"\n",
    );

    // The dependency's declared output points at a real file so the copy
    // task that proves propagation has something to read.
    let lib_input = project.dir.join("lib/in.txt");
    std::fs::create_dir_all(project.dir.join("lib")).expect("lib dir");
    std::fs::write(&lib_input, "lib").expect("lib input");
    project.write(
        "lib/build.ulb",
        &format!(
            "jvm {{ jarFile = {:?} }}\n\
             deps {{\n  api \"com.example:shared:1.0\"\n}}\n\
             source = {:?}\noutput = {:?}\n",
            bridge.join("bridge-1.0.jar").display().to_string(),
            lib_input.display().to_string(),
            project.dir.join("lib/out.txt").display().to_string(),
        ),
    );

    let app_input = project.dir.join("app/in.txt");
    std::fs::create_dir_all(project.dir.join("app")).expect("app dir");
    std::fs::write(&app_input, "app").expect("app input");
    let copied = project.dir.join("app/copied-shared.jar");
    project.write(
        "app/build.ulb",
        &format!(
            "commonMain.deps {{\n  api project(\":lib\")\n}}\n\
             sourceSetClasspath {{\n  name = \"commonMain\"\n  index = 1\n  output = {:?}\n}}\n\
             source = {:?}\noutput = {:?}\n",
            copied.display().to_string(),
            app_input.display().to_string(),
            project.dir.join("app/out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.repos = Some(vec![MavenRepo::Custom(
        project.dir.join("repo").display().to_string(),
    )]);
    let first = build_project(&project.dir, &options).expect("first build");
    // app: stage, announce, source-set classpath copy; lib: stage, announce.
    assert_eq!((first.ran, first.up_to_date), (5, 0));
    // Index 1 of app's commonMain compile bucket is lib's api-scoped
    // `shared` jar — index 0 is lib's own output.
    assert_eq!(project.read("app/copied-shared.jar"), b"shared jar");

    let second = build_project(&project.dir, &options).expect("second build");
    assert_eq!((second.ran, second.up_to_date), (0, 5));
}

/// An `implementation`-scoped project reference carries only the depended
/// module's output — its api-scoped jars must not leak into the consuming
/// source set, so a probe asking for index 1 finds nothing at configure
/// time.
#[test]
fn source_set_project_refs_implementation_does_not_propagate_api_jars() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let project = TestProject::new("source-set-impl-refs", &fixture);

    let bridge = write_artifact(&project, "com/example", "bridge", b"bridge jar");
    write_artifact(&project, "com/example", "shared", b"shared jar");

    project.write(
        "settings.ulb",
        "project \"ImplRefs\"\nmodule \"app\"\nmodule \"lib\"\n",
    );

    let lib_input = project.dir.join("lib/in.txt");
    std::fs::create_dir_all(project.dir.join("lib")).expect("lib dir");
    std::fs::write(&lib_input, "lib").expect("lib input");
    project.write(
        "lib/build.ulb",
        &format!(
            "jvm {{ jarFile = {:?} }}\n\
             deps {{\n  api \"com.example:shared:1.0\"\n}}\n\
             source = {:?}\noutput = {:?}\n",
            bridge.join("bridge-1.0.jar").display().to_string(),
            lib_input.display().to_string(),
            project.dir.join("lib/out.txt").display().to_string(),
        ),
    );

    let app_input = project.dir.join("app/in.txt");
    std::fs::create_dir_all(project.dir.join("app")).expect("app dir");
    std::fs::write(&app_input, "app").expect("app input");
    project.write(
        "app/build.ulb",
        &format!(
            "commonMain.deps {{\n  implementation project(\":lib\")\n}}\n\
             sourceSetClasspath {{\n  name = \"commonMain\"\n  index = 1\n  output = {:?}\n}}\n\
             source = {:?}\noutput = {:?}\n",
            project.dir.join("app/copied.jar").display().to_string(),
            app_input.display().to_string(),
            project.dir.join("app/out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );

    let mut options = project.options();
    options.repos = Some(vec![MavenRepo::Custom(
        project.dir.join("repo").display().to_string(),
    )]);
    let error = build_project(&project.dir, &options).expect_err("api jars leaked");
    assert!(
        error.contains("no compile jar at index 1 for source set 'commonMain'"),
        "{error}"
    );
}

/// Two plugins with a declared cross-plugin dependency, configured by the
/// real driver: the consumer's task references `ulite/fixture:stage`, and
/// the provider's tasks live under a different module label
/// (`ulite/fixture`) than the consumer's (`ulite/cross-dep-fixture`).
/// Scheduling must resolve the reference across those labels.
#[test]
fn cross_plugin_task_refs_are_scheduled_through_the_driver() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let consumer = build_fixture("ulb-plugin-cross-dep-fixture");
    let project = TestProject::new("cross-driver", &fixture);

    // Extend the one-entry index with the consumer plugin.
    let index = serde_json::json!({
        "schema_version": 1,
        "plugins": {
            "ulite/fixture": {
                "versions": {
                    "0.1.0": {
                        "abi": { "min": "0.4", "max": "0.7" },
                        "artifact_url": fixture.display().to_string(),
                    }
                }
            },
            "ulite/cross-dep-fixture": {
                "versions": {
                    "0.1.0": {
                        "abi": { "min": "0.4", "max": "0.7" },
                        "artifact_url": consumer.display().to_string(),
                    }
                }
            }
        }
    });
    std::fs::write(project.dir.join("index.json"), index.to_string()).expect("rewrite index");

    project.write(
        "build.ulb",
        &format!(
            "source = {:?}\noutput = {:?}\nconsumeFrom = \"ulite/fixture:stage\"\n",
            project.dir.join("in.txt").display().to_string(),
            project.dir.join("out.txt").display().to_string(),
        ),
    );
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n  consumer = \"ulite/cross-dep-fixture\" @ \"0.1.0\"\n}\n",
    );

    let options = project.options();
    let first = build_project(&project.dir, &options).expect("first build");
    // stage, announce (provider) and consume (consumer) all ran.
    assert_eq!((first.ran, first.up_to_date), (3, 0));
    let second = build_project(&project.dir, &options).expect("second build");
    assert_eq!((second.ran, second.up_to_date), (0, 3));
}

#[test]
fn variant_selection_restricts_registered_tasks() {
    let fixture = build_fixture("ulb-plugin-fixture");
    let build_ulb = "source = \"in.txt\"\n\
output = \"out.txt\"\n\
variantProbe true\n\
buildTypes {\n\
  debug { minifyEnabled false }\n\
  release { minifyEnabled true }\n\
}\n\
productFlavors {\n\
  dimension \"tier\"\n\
  free { applicationIdSuffix \".free\" }\n\
  paid { applicationIdSuffix \".paid\" }\n\
}\n";
    let libs_ulb = "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n";

    // Without selection, the full matrix is registered: stage + announce
    // plus one probe per variant (debug/release × free/paid).
    let full = TestProject::new("variant-full", &fixture);
    full.write("build.ulb", build_ulb);
    full.write("libs.ulb", libs_ulb);
    let all = build_project(&full.dir, &full.options()).expect("full build");
    assert_eq!((all.ran, all.up_to_date), (6, 0));

    // Restricted to a single variant: only that probe joins the two
    // always-registered fixture tasks.
    let single = TestProject::new("variant-single", &fixture);
    single.write("build.ulb", build_ulb);
    single.write("libs.ulb", libs_ulb);
    let options = BuildOptions {
        variants: Some(vec!["freeDebug".to_owned()]),
        ..single.options()
    };
    let selected = build_project(&single.dir, &options).expect("restricted build");
    assert_eq!((selected.ran, selected.up_to_date), (3, 0));
    let rerun = build_project(&single.dir, &options).expect("rebuild");
    assert_eq!((rerun.ran, rerun.up_to_date), (0, 3));

    // An unknown variant fails with the valid set named.
    let bad = build_project(
        &single.dir,
        &BuildOptions {
            variants: Some(vec!["turbo".to_owned()]),
            ..single.options()
        },
    )
    .expect_err("unknown variant must fail");
    assert!(
        bad.contains("unknown variant 'turbo'")
            && bad.contains("DebugFree")
            && bad.contains("ReleasePaid"),
        "{bad}"
    );
}

#[test]
fn variant_selection_is_consistent_across_modules() {
    let fixture = build_fixture("ulb-plugin-fixture");

    // A flavored consumer depending on a flavor-less provider through
    // settings.ulb + project(":lib"). Selecting the consumer's freeDebug
    // must restrict BOTH modules consistently: the provider (no flavors)
    // resolves its build-type component and registers only probeDebug.
    let project = TestProject::new("variant-multi", &fixture);
    std::fs::create_dir_all(project.dir.join("app")).expect("app dir");
    std::fs::create_dir_all(project.dir.join("lib")).expect("lib dir");
    // Each module's `source` resolves against ITS OWN directory (the
    // host injects the module path as projectDir), so both need a copy.
    std::fs::write(project.dir.join("app/in.txt"), "app input").expect("write app input");
    std::fs::write(project.dir.join("lib/in.txt"), "lib input").expect("write lib input");
    project.write(
        "settings.ulb",
        "project \"VariantMulti\"\nmodule \"app\"\nmodule \"lib\"\n",
    );
    project.write("conventions.ulb", "");
    project.write(
        "libs.ulb",
        "plugins {\n  fixture = \"ulite/fixture\" @ \"0.1.0\"\n}\n",
    );
    project.write(
        "app/build.ulb",
        "source = \"in.txt\"\n\
output = \"out.txt\"\n\
variantProbe true\n\
deps {\n\
  implementation project(\":lib\")\n\
}\n\
buildTypes {\n\
  debug { minifyEnabled false }\n\
  release { minifyEnabled true }\n\
}\n\
productFlavors {\n\
  dimension \"tier\"\n\
  free { applicationIdSuffix \".free\" }\n\
  paid { applicationIdSuffix \".paid\" }\n\
}\n",
    );
    project.write(
        "lib/build.ulb",
        "source = \"in.txt\"\n\
output = \"lib-out.txt\"\n\
variantProbe true\n\
jvm {\n\
  jarFile \"build/lib.jar\"\n\
}\n",
    );

    // Full matrices: app has six tasks, lib has four.
    let all = build_project(&project.dir, &project.options()).expect("full build");
    assert!(all.failure.is_none(), "{:?}", all.failure);
    assert_eq!((all.ran, all.up_to_date), (10, 0));

    // freeDebug: app keeps debug+free; lib resolves the Debug component.
    let options = BuildOptions {
        variants: Some(vec!["freeDebug".to_owned()]),
        ..project.options()
    };
    let selected = build_project(&project.dir, &options).expect("restricted build");
    assert!(selected.failure.is_none(), "{:?}", selected.failure);
    assert_eq!((selected.ran, selected.up_to_date), (6, 0));
    let rerun = build_project(&project.dir, &options).expect("rebuild");
    assert_eq!((rerun.ran, rerun.up_to_date), (0, 6));
}
