//! Project scaffolding for `uliab init` (ARCHITECTURE.md §13).
//!
//! Generates the four core `.ulb` files (`settings.ulb`, `libs.ulb`,
//! `build.ulb`, `conventions.ulb`) and the standard source directory
//! structure for a new project. Each project type (JVM, Android, KMP)
//! produces a minimal but valid starting configuration that the build
//! tool can evaluate without errors.

use std::path::{Path, PathBuf};

/// The supported project types for `uliab init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectType {
    /// Plain Java/Kotlin/JVM module.
    Jvm,
    /// Android application module.
    Android,
    /// Kotlin Multiplatform module (JVM target).
    Kmp,
}

impl ProjectType {
    /// Parses a string into a [`ProjectType`].
    ///
    /// # Errors
    ///
    /// Returns an error message when the string is not a recognized type.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "jvm" => Ok(Self::Jvm),
            "android" => Ok(Self::Android),
            "kmp" => Ok(Self::Kmp),
            other => Err(format!(
                "unknown project type '{other}' (expected: jvm, android, kmp)"
            )),
        }
    }
}

/// The four core files a scaffolded project contains.
pub struct ProjectFiles {
    /// The `settings.ulb` content.
    pub settings: String,
    /// The `libs.ulb` content.
    pub libs: String,
    /// The `build.ulb` content (written under the module directory).
    pub build: String,
    /// The `conventions.ulb` content.
    pub conventions: String,
}

/// Generates the file contents for a project of the given type.
///
/// `module_dir` is the subdirectory name for the module (e.g. `"app"`).
/// `namespace` is the Android package name for Android projects; ignored
/// for other types.
#[must_use]
pub fn generate(project_type: ProjectType, module_dir: &str, namespace: &str) -> ProjectFiles {
    match project_type {
        ProjectType::Jvm => generate_jvm(module_dir),
        ProjectType::Android => generate_android(module_dir, namespace),
        ProjectType::Kmp => generate_kmp(module_dir),
    }
}

fn generate_jvm(module_dir: &str) -> ProjectFiles {
    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!("plugins {\n", "  jvm = \"ulite/jvm\" @ \"0.5.0\"\n", "}\n",).to_string(),
        build: concat!(
            "plugin \"jvm\"\n",
            "\n",
            "jvm {\n",
            "  sources [ \"src/main/java\" ]\n",
            "  classesDir \"build/classes\"\n",
            "  jarFile \"build/app.jar\"\n",
            "}\n",
        )
        .to_string(),
        conventions: String::new(),
    }
}

fn generate_android(module_dir: &str, namespace: &str) -> ProjectFiles {
    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!(
            "plugins {\n",
            "  android = \"ulite/android\" @ \"0.2.0\"\n",
            "}\n",
        )
        .to_string(),
        build: format!(
            r#"plugin "android"

android {{
  namespace "{namespace}"
  compileSdk 36
  minSdk 24
  sources [ "src/main/java" ]
  resDir "src/main/res"
  manifest "src/main/AndroidManifest.xml"
  apk "build/app.apk"
}}
"#
        ),
        conventions: String::new(),
    }
}

fn generate_kmp(module_dir: &str) -> ProjectFiles {
    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!("plugins {\n", "  kmp = \"ulite/kmp\" @ \"0.1.0\"\n", "}\n",).to_string(),
        build: concat!(
            "plugin \"kmp\"\n",
            "\n",
            "kmp {\n",
            "  commonMain {\n",
            "    sources [ \"src/commonMain/kotlin\" ]\n",
            "  }\n",
            "  jvmMain {\n",
            "    sources [ \"src/jvmMain/kotlin\" ]\n",
            "    classesDir \"build/jvm/classes\"\n",
            "    jarFile \"build/jvm/app.jar\"\n",
            "  }\n",
            "}\n",
        )
        .to_string(),
        conventions: String::new(),
    }
}

/// The directories to create for a project type.
fn source_dirs(project_type: ProjectType, module_dir: &str) -> Vec<PathBuf> {
    let base = PathBuf::from(module_dir);
    match project_type {
        ProjectType::Jvm => vec![base.join("src/main/java")],
        ProjectType::Android => vec![base.join("src/main/java"), base.join("src/main/res/values")],
        ProjectType::Kmp => vec![
            base.join("src/commonMain/kotlin"),
            base.join("src/jvmMain/kotlin"),
        ],
    }
}

/// Scaffolds a new project on disk.
///
/// Creates the directory structure and writes all four `.ulb` files.
/// Returns an error if `settings.ulb` already exists in the target
/// directory (safety check) or if any file operation fails.
///
/// # Errors
///
/// Returns an error when the target directory already contains a
/// `settings.ulb`, or when a file cannot be created.
pub fn scaffold(
    target_dir: &Path,
    project_type: ProjectType,
    module_dir: &str,
    namespace: &str,
) -> Result<Vec<PathBuf>, String> {
    if target_dir.join("settings.ulb").exists() {
        return Err(format!(
            "'{}' already contains a settings.ulb — aborting to avoid overwriting",
            target_dir.display()
        ));
    }

    let files = generate(project_type, module_dir, namespace);
    let mut created = Vec::new();

    // Write the four core files at the project root.
    for (name, content) in [
        ("settings.ulb", &files.settings),
        ("libs.ulb", &files.libs),
        ("conventions.ulb", &files.conventions),
    ] {
        let path = target_dir.join(name);
        std::fs::write(&path, content)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        created.push(path);
    }

    // build.ulb goes inside the module directory — ensure the dir exists first.
    let module_path = target_dir.join(module_dir);
    std::fs::create_dir_all(&module_path)
        .map_err(|error| format!("creating {}: {error}", module_path.display()))?;
    let build_path = module_path.join("build.ulb");
    std::fs::write(&build_path, &files.build)
        .map_err(|error| format!("writing {}: {error}", build_path.display()))?;
    created.push(build_path.clone());

    // Create source directories.
    for dir in source_dirs(project_type, module_dir) {
        let path = target_dir.join(&dir);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("creating {}: {error}", path.display()))?;
    }

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_type_parse_valid() {
        assert_eq!(ProjectType::parse("jvm").unwrap(), ProjectType::Jvm);
        assert_eq!(ProjectType::parse("android").unwrap(), ProjectType::Android);
        assert_eq!(ProjectType::parse("kmp").unwrap(), ProjectType::Kmp);
        assert_eq!(ProjectType::parse("JVM").unwrap(), ProjectType::Jvm);
        assert_eq!(ProjectType::parse("Android").unwrap(), ProjectType::Android);
    }

    #[test]
    fn project_type_parse_invalid() {
        let error = ProjectType::parse("ios").unwrap_err();
        assert!(error.contains("unknown project type"), "{error}");
    }

    #[test]
    fn jvm_templates_parse_cleanly() {
        let files = generate(ProjectType::Jvm, "app", "");
        let settings = ulb_lang::eval::evaluate_settings(&files.settings);
        assert!(
            settings.diagnostics.is_empty(),
            "{:?}",
            settings.diagnostics
        );
        assert_eq!(settings.model.project_name.as_deref(), Some("MyApp"));
        assert_eq!(settings.model.modules, vec!["app"]);

        let parsed = ulb_lang::parse(&files.libs);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    }

    #[test]
    fn android_templates_parse_cleanly() {
        let files = generate(ProjectType::Android, "app", "com.example.test");
        let settings = ulb_lang::eval::evaluate_settings(&files.settings);
        assert!(
            settings.diagnostics.is_empty(),
            "{:?}",
            settings.diagnostics
        );
        assert_eq!(settings.model.project_name.as_deref(), Some("MyApp"));
        assert_eq!(settings.model.modules, vec!["app"]);

        let parsed = ulb_lang::parse(&files.libs);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);

        let parsed = ulb_lang::parse(&files.build);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    }

    #[test]
    fn kmp_templates_parse_cleanly() {
        let files = generate(ProjectType::Kmp, "app", "");
        let settings = ulb_lang::eval::evaluate_settings(&files.settings);
        assert!(
            settings.diagnostics.is_empty(),
            "{:?}",
            settings.diagnostics
        );

        let parsed = ulb_lang::parse(&files.libs);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);

        let parsed = ulb_lang::parse(&files.build);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    }

    #[test]
    fn android_build_contains_namespace() {
        let files = generate(ProjectType::Android, "app", "com.example.myapp");
        assert!(files.build.contains("com.example.myapp"));
    }

    #[test]
    fn scaffold_refuses_overwrite() {
        let dir =
            std::env::temp_dir().join(format!("uliab-init-overwrite-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("settings.ulb"), "project \"Old\"\n").unwrap();

        let result = scaffold(&dir, ProjectType::Jvm, "app", "");
        assert!(result.is_err(), "should refuse to overwrite");
        assert!(
            result.unwrap_err().contains("already contains"),
            "error should mention existing file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_creates_all_files() {
        let dir =
            std::env::temp_dir().join(format!("uliab-init-scaffold-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let created = scaffold(&dir, ProjectType::Jvm, "app", "").expect("scaffold");
        assert!(created.len() >= 4, "should create at least 4 files");

        assert!(dir.join("settings.ulb").exists());
        assert!(dir.join("libs.ulb").exists());
        assert!(dir.join("conventions.ulb").exists());
        assert!(dir.join("app/build.ulb").exists());
        assert!(dir.join("app/src/main/java").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_android_creates_res_dir() {
        let dir =
            std::env::temp_dir().join(format!("uliab-init-android-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        scaffold(&dir, ProjectType::Android, "app", "com.example.test").expect("scaffold");

        assert!(dir.join("app/src/main/java").is_dir());
        assert!(dir.join("app/src/main/res/values").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_kmp_creates_common_and_jvm_dirs() {
        let dir = std::env::temp_dir().join(format!("uliab-init-kmp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        scaffold(&dir, ProjectType::Kmp, "app", "").expect("scaffold");

        assert!(dir.join("app/src/commonMain/kotlin").is_dir());
        assert!(dir.join("app/src/jvmMain/kotlin").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_with_custom_module_dir() {
        let dir =
            std::env::temp_dir().join(format!("uliab-init-custom-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        scaffold(&dir, ProjectType::Jvm, "mylib", "").expect("scaffold");

        assert!(dir.join("mylib/build.ulb").exists());
        assert!(dir.join("mylib/src/main/java").is_dir());

        let settings = std::fs::read_to_string(dir.join("settings.ulb")).unwrap();
        assert!(settings.contains("module \"mylib\""));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
