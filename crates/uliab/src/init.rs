//! Project scaffolding for `uliab init` (ARCHITECTURE.md §13).
//!
//! Generates the four core `.ulb` files (`settings.ulb`, `libs.ulb`,
//! `build.ulb`, `conventions.ulb`) and the standard source directory
//! structure for a new project. Each project type (JVM, Android, KMP)
//! produces a minimal but valid starting configuration that the build
//! tool can evaluate without errors. Scaffolded projects include starter
//! source files so the first build succeeds without manual editing.

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

/// The four core files a scaffolded project contains, plus any additional
/// scaffold files (starter sources, manifest, etc.) that the project type
/// needs for its first build to succeed.
pub struct ProjectFiles {
    /// The `settings.ulb` content.
    pub settings: String,
    /// The `libs.ulb` content.
    pub libs: String,
    /// The `build.ulb` content (written under the module directory).
    pub build: String,
    /// The `conventions.ulb` content.
    pub conventions: String,
    /// Additional files to create: `(relative_path, content)` pairs.
    /// Paths are relative to the project root.
    pub scaffold_files: Vec<(String, String)>,
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
    let src = format!("{module_dir}/src/main/kotlin/Main.kt");
    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!("plugins {\n", "  jvm = \"ulite/jvm\" @ \"0.6.0\"\n", "}\n",).to_string(),
        build: concat!(
            "plugin \"jvm\"\n",
            "\n",
            "jvm {\n",
            "  sources [ \"src/main/kotlin/Main.kt\" ]\n",
            "  classesDir \"build/classes\"\n",
            "  jarFile \"build/app.jar\"\n",
            "}\n",
        )
        .to_string(),
        conventions: String::new(),
        scaffold_files: vec![(
            src,
            "fun main() {\n    println(\"Hello from {module_dir}\")\n}\n"
                .replace("{module_dir}", module_dir),
        )],
    }
}

fn generate_android(module_dir: &str, namespace: &str) -> ProjectFiles {
    let pkg_path = namespace.replace('.', "/");
    let main_activity = format!("{module_dir}/src/main/java/{pkg_path}/MainActivity.java");
    let manifest = format!("{module_dir}/src/main/AndroidManifest.xml");
    let res_values = format!("{module_dir}/src/main/res/values/strings.xml");

    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!(
            "plugins {\n",
            "  android = \"ulite/android\" @ \"0.3.0\"\n",
            "}\n",
        )
        .to_string(),
        build: format!(
            r#"plugin "android"

android {{
  namespace "{namespace}"
  compileSdk 36
  minSdk 24
  sources [ "src/main/java/{pkg_path}/MainActivity.java" ]
  resDir "src/main/res"
  manifest "src/main/AndroidManifest.xml"
}}
"#,
            namespace = namespace,
            pkg_path = pkg_path,
        ),
        conventions: String::new(),
        scaffold_files: vec![
            (
                manifest,
                format!(
                    r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="{namespace}">
    <application android:label="@string/app_name">
        <activity android:name=".{pkg_path}.MainActivity"
            android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
"#,
                ),
            ),
            (
                res_values,
                "<resources>\n    <string name=\"app_name\">MyApp</string>\n</resources>\n"
                    .to_string(),
            ),
            (
                main_activity,
                format!(
                    r#"package {namespace};

import android.app.Activity;
import android.os.Bundle;

public class MainActivity extends Activity {{
    @Override
    protected void onCreate(Bundle savedInstanceState) {{
        super.onCreate(savedInstanceState);
        setContentView(R.layout.activity_main);
    }}
}}
"#,
                ),
            ),
        ],
    }
}

fn generate_kmp(module_dir: &str) -> ProjectFiles {
    let common_src = format!("{module_dir}/src/commonMain/kotlin/Platform.kt");
    let jvm_src = format!("{module_dir}/src/jvmMain/kotlin/Main.kt");
    ProjectFiles {
        settings: format!(
            r#"project "MyApp"
module "{module_dir}"
"#
        ),
        libs: concat!(
            "plugins {\n",
            "  kmp = \"ulite/kmp\" @ \"0.3.0\"\n",
            "}\n",
        )
        .to_string(),
        build: concat!(
            "plugin \"kmp\"\n",
            "\n",
            "kmp {\n",
            "  commonMain {\n",
            "    sources [ \"src/commonMain/kotlin/Platform.kt\" ]\n",
            "  }\n",
            "  jvmMain {\n",
            "    sources [ \"src/jvmMain/kotlin/Main.kt\" ]\n",
            "  }\n",
            "  jvm {\n",
            "    classesDir \"build/jvm/classes\"\n",
            "    jarFile \"build/jvm/app.jar\"\n",
            "  }\n",
            "}\n",
        )
        .to_string(),
        conventions: String::new(),
        scaffold_files: vec![
            (common_src, "expect class Platform {\n    fun name(): String\n}\n".to_string()),
            (
                jvm_src,
                "actual class Platform {\n    actual fun name(): String = \"JVM\"\n}\n\nfun main() {\n    println(\"Hello from JVM: ${Platform().name()}\")\n}\n"
                    .to_string(),
            ),
        ],
    }
}

/// The directories to create for a project type.
fn source_dirs(project_type: ProjectType, module_dir: &str) -> Vec<PathBuf> {
    let base = PathBuf::from(module_dir);
    match project_type {
        ProjectType::Jvm => vec![base.join("src/main/kotlin")],
        ProjectType::Android => vec![
            base.join("src/main/res/layout"),
            base.join("src/main/res/values"),
        ],
        ProjectType::Kmp => vec![
            base.join("src/commonMain/kotlin"),
            base.join("src/jvmMain/kotlin"),
        ],
    }
}

/// Scaffolds a new project on disk.
///
/// Creates the directory structure and writes all four `.ulb` files plus
/// any scaffold files the project type needs. Returns an error if
/// `settings.ulb` already exists in the target directory (safety check)
/// or if any file operation fails.
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
    created.push(build_path);

    // Create source directories.
    for dir in source_dirs(project_type, module_dir) {
        let path = target_dir.join(&dir);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("creating {}: {error}", path.display()))?;
    }

    // Write scaffold files (starter sources, manifests, etc.).
    for (relative, content) in &files.scaffold_files {
        let path = target_dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        created.push(path);
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

        let parsed = ulb_lang::parse(&files.build);
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
    fn android_template_has_no_apk_key() {
        let files = generate(ProjectType::Android, "app", "com.example.test");
        assert!(
            !files.build.contains("apk"),
            "android template should not contain an 'apk' key"
        );
    }

    #[test]
    fn android_template_scaffold_files_exist() {
        let files = generate(ProjectType::Android, "app", "com.example.test");
        let paths: Vec<&str> = files
            .scaffold_files
            .iter()
            .map(|(p, _)| p.as_str())
            .collect();
        assert!(paths.iter().any(|p| p.contains("AndroidManifest.xml")));
        assert!(paths.iter().any(|p| p.contains("strings.xml")));
        assert!(paths.iter().any(|p| p.contains("MainActivity.java")));
    }

    #[test]
    fn kmp_template_jvm_classes_dir_in_target_block() {
        let files = generate(ProjectType::Kmp, "app", "");
        assert!(
            files.build.contains("jvm {\n"),
            "KMP template must have a separate 'jvm' target block"
        );
        assert!(
            files.build.contains("classesDir"),
            "KMP template must contain classesDir"
        );
        assert!(
            files.build.contains("jarFile"),
            "KMP template must contain jarFile"
        );
    }

    #[test]
    fn jvm_template_scaffold_file_is_kotlin() {
        let files = generate(ProjectType::Jvm, "app", "");
        assert_eq!(files.scaffold_files.len(), 1);
        assert!(files.scaffold_files[0].0.ends_with("Main.kt"));
        assert!(files.scaffold_files[0].1.contains("fun main()"));
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
        assert!(dir.join("app/src/main/kotlin").is_dir());
        assert!(dir.join("app/src/main/kotlin/Main.kt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scaffold_android_creates_manifest_and_sources() {
        let dir =
            std::env::temp_dir().join(format!("uliab-init-android-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        scaffold(&dir, ProjectType::Android, "app", "com.example.test").expect("scaffold");

        assert!(dir.join("app/src/main/res/layout").is_dir());
        assert!(dir.join("app/src/main/res/values").is_dir());
        assert!(dir.join("app/src/main/res/values/strings.xml").exists());
        assert!(dir.join("app/src/main/AndroidManifest.xml").exists());
        assert!(
            dir.join("app/src/main/java/com/example/test/MainActivity.java")
                .exists()
        );

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
        assert!(dir.join("app/src/commonMain/kotlin/Platform.kt").exists());
        assert!(dir.join("app/src/jvmMain/kotlin/Main.kt").exists());

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
        assert!(dir.join("mylib/src/main/kotlin").is_dir());

        let settings = std::fs::read_to_string(dir.join("settings.ulb")).unwrap();
        assert!(settings.contains("module \"mylib\""));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
