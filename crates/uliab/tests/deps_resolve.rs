//! End-to-end dependency resolution: a project's `deps {}` block becomes a
//! real classpath of jar files (ARCHITECTURE.md §6, §7). These tests stay
//! offline by pointing the resolver at a local Maven repository layout
//! written by the test itself.

use std::fs;
use std::path::PathBuf;

use uliab::driver::{resolve_project_deps, resolve_project_source_sets};
use uliab::maven::MavenRepo;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_unique(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "uliab-deps-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

struct LocalRepo {
    root: PathBuf,
}

impl LocalRepo {
    fn new() -> LocalRepo {
        let root = next_unique("repo");
        fs::create_dir_all(&root).unwrap();
        LocalRepo { root }
    }

    fn add(&self, group: &str, artifact: &str, version: &str, children: &[(&str, &str, &str)]) {
        let mut pom = format!(
            "<?xml version=\"1.0\"?><project><modelVersion>4.0.0</modelVersion>\
             <groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>{version}</version>"
        );
        if !children.is_empty() {
            pom.push_str("<dependencies>");
            for (child, child_version, scope) in children {
                let child_group = child.split(':').next().unwrap();
                let child_artifact = child.split(':').nth(1).unwrap();
                let scope_tag = if scope.is_empty() {
                    String::new()
                } else {
                    format!("<scope>{scope}</scope>")
                };
                pom.push_str(&format!(
                    "<dependency><groupId>{child_group}</groupId>\
                     <artifactId>{child_artifact}</artifactId><version>{child_version}</version>{scope_tag}</dependency>"
                ));
            }
            pom.push_str("</dependencies>");
        }
        pom.push_str("</project>");

        let rel = format!(
            "{}/{}/{}/{}-{}",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version
        );
        fs::create_dir_all(self.root.join(&rel)).unwrap();
        fs::write(self.root.join(format!("{rel}.pom")), pom).unwrap();
        fs::write(
            self.root.join(format!("{rel}.jar")),
            format!("{artifact}-{version}"),
        )
        .unwrap();
    }
}

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(build_ulb: &str) -> Project {
        let dir = next_unique("project");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("libs.ulb"), "").unwrap();
        fs::write(dir.join("build.ulb"), build_ulb).unwrap();
        Project { dir }
    }
}

fn jar_names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn project_dir_is_a_real_classpath(paths: &[PathBuf]) {
    assert!(!paths.is_empty());
    for path in paths {
        assert!(path.is_file(), "{} is not a file on disk", path.display());
    }
}

#[test]
fn deps_block_resolves_to_jars_on_disk() {
    let repo = LocalRepo::new();
    repo.add(
        "com.example",
        "one",
        "1.0",
        &[("com.example:two", "1.0", "")],
    );
    repo.add("com.example", "two", "1.0", &[]);
    let project = Project::new("deps {\n  implementation \"com.example:one:1.0\"\n}\n");

    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolution = resolve_project_deps(&project.dir, &repos, Some(project.dir.join(".cache")))
        .expect("resolves");
    project_dir_is_a_real_classpath(&resolution.classpath.compile);
    assert_eq!(
        jar_names(&resolution.classpath.compile),
        vec!["one-1.0.jar", "two-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.runtime),
        vec!["one-1.0.jar", "two-1.0.jar"]
    );
    assert!(resolution.classpath.processor.is_empty());
}

#[test]
fn scopes_land_in_their_buckets() {
    let repo = LocalRepo::new();
    repo.add("com.example", "lib", "1.0", &[]);
    repo.add("com.example", "rt", "1.0", &[]);
    repo.add("com.example", "proc", "1.0", &[]);
    repo.add("com.example", "t", "1.0", &[]);
    repo.add("com.example", "at", "1.0", &[]);
    let project = Project::new(
        "deps {\n\
            api \"com.example:lib:1.0\"\n\
            runtimeOnly \"com.example:rt:1.0\"\n\
            ksp \"com.example:proc:1.0\"\n\
            testImplementation \"com.example:t:1.0\"\n\
            androidTestImplementation \"com.example:at:1.0\"\n\
        }\n",
    );

    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolution = resolve_project_deps(&project.dir, &repos, Some(project.dir.join(".cache")))
        .expect("resolves");
    assert_eq!(
        jar_names(&resolution.classpath.compile),
        vec!["lib-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.runtime),
        vec!["lib-1.0.jar", "rt-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.processor),
        vec!["proc-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.test_compile),
        vec!["lib-1.0.jar", "t-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.test_runtime),
        vec!["lib-1.0.jar", "rt-1.0.jar", "t-1.0.jar"]
    );
    assert_eq!(
        jar_names(&resolution.classpath.android_test_compile),
        vec!["at-1.0.jar", "lib-1.0.jar"]
    );
}

#[test]
fn conflicting_versions_keep_the_highest() {
    let repo = LocalRepo::new();
    repo.add(
        "com.example",
        "app",
        "1.0",
        &[("com.example:lib", "1.0", "")],
    );
    repo.add("com.example", "lib", "1.0", &[]);
    repo.add("com.example", "lib", "2.0", &[]);
    let project = Project::new(
        "deps {\n\
            implementation \"com.example:app:1.0\"\n\
            implementation \"com.example:lib:2.0\"\n\
        }\n",
    );

    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolution = resolve_project_deps(&project.dir, &repos, Some(project.dir.join(".cache")))
        .expect("resolves");
    assert_eq!(
        jar_names(&resolution.classpath.compile),
        vec!["app-1.0.jar", "lib-2.0.jar"]
    );
    assert!(
        resolution
            .notes
            .iter()
            .any(|note| note.contains("com.example:lib:1.0 superseded")),
        "notes: {:?}",
        resolution.notes
    );
}

#[test]
fn missing_deps_block_is_an_error() {
    let repo = LocalRepo::new();
    let project = Project::new("android {\n  compileSdk = 35\n}\n");
    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let error = resolve_project_deps(&project.dir, &repos, None).expect_err("no deps block");
    assert!(error.contains("does not declare a deps"), "{error}");
}

#[test]
fn missing_artifact_is_an_error() {
    let repo = LocalRepo::new();
    let project = Project::new("deps {\n  implementation \"com.example:absent:1.0\"\n}\n");
    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let error = resolve_project_deps(&project.dir, &repos, None).expect_err("missing artifact");
    assert!(error.contains("could not find"), "{error}");
}

#[test]
fn source_set_deps_resolve_independently() {
    let repo = LocalRepo::new();
    repo.add("com.example", "common", "1.0", &[]);
    repo.add("com.example", "android", "1.0", &[]);
    let project = Project::new(
        "commonMain.deps {\n  implementation \"com.example:common:1.0\"\n}\n\
         androidMain.deps {\n  implementation \"com.example:android:1.0\"\n}\n",
    );

    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolved =
        resolve_project_source_sets(&project.dir, &repos, Some(project.dir.join(".cache")))
            .expect("resolves");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].0, "androidMain");
    assert_eq!(jar_names(&resolved[0].1.compile), vec!["android-1.0.jar"]);
    assert!(resolved[0].1.runtime.contains(&resolved[0].1.compile[0]));
    assert_eq!(resolved[1].0, "commonMain");
    assert_eq!(jar_names(&resolved[1].1.compile), vec!["common-1.0.jar"]);
    assert!(resolved[1].1.runtime.contains(&resolved[1].1.compile[0]));
}

#[test]
fn nested_source_set_deps_resolve_by_full_path() {
    let repo = LocalRepo::new();
    repo.add("com.example", "shared", "1.0", &[]);
    let project = Project::new(
        "kmp {\n  commonMain.deps {\n    implementation \"com.example:shared:1.0\"\n  }\n}\n",
    );

    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolved =
        resolve_project_source_sets(&project.dir, &repos, Some(project.dir.join(".cache")))
            .expect("resolves");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, "kmp.commonMain");
    assert_eq!(jar_names(&resolved[0].1.compile), vec!["shared-1.0.jar"]);
}

#[test]
fn a_model_without_source_set_deps_resolves_to_nothing() {
    let repo = LocalRepo::new();
    let project = Project::new("commonMain { sources [\"src/commonMain\"] }\n");
    let repos = vec![MavenRepo::Custom(repo.root.display().to_string())];
    let resolved = resolve_project_source_sets(&project.dir, &repos, None).expect("no deps");
    assert!(resolved.is_empty());
}
