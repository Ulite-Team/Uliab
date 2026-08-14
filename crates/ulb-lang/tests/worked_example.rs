//! End-to-end evaluation of the `examples/sample-kmp/` worked example.
//!
//! Reads the three source files plus `signing.properties` from the example
//! directory, injects the environment and properties the `signing` block
//! needs (see [`ulb_lang::eval::EvalEnvironment`]), and asserts the
//! resolved module model reflects GRAMMAR.md §6–§10 semantics: convention
//! `apply` merging into block targets, catalog aliases resolving to
//! coordinates, repeated scalar pairs accumulating, and task `run` actions
//! captured as data.

use std::collections::BTreeMap;

use ulb_lang::eval::{EvalEnvironment, Value, evaluate_project_with};
use ulb_lang::token::Number;

const CONVENTIONS: &str = include_str!("../../../examples/sample-kmp/conventions.ulb");
const LIBS: &str = include_str!("../../../examples/sample-kmp/libs.ulb");
const BUILD: &str = include_str!("../../../examples/sample-kmp/build.ulb");
const SIGNING_PROPERTIES: &str = include_str!("../../../examples/sample-kmp/signing.properties");

fn block<'a>(map: &'a BTreeMap<String, Value>, key: &str) -> &'a BTreeMap<String, Value> {
    match &map[key] {
        Value::Block(inner) => inner,
        other => panic!("expected {key} to be a Block, got {other:?}"),
    }
}

fn coordinate_list(map: &BTreeMap<String, Value>, key: &str) -> Vec<Value> {
    match &map[key] {
        Value::List(items) => items.clone(),
        other => panic!("expected {key} to be a List, got {other:?}"),
    }
}

#[test]
fn sample_kmp_evaluates_to_expected_model() {
    let mut env = EvalEnvironment::default();
    env.env
        .insert("STORE_PASSWORD".to_owned(), "hunter2".to_owned());
    env.env
        .insert("KEY_PASSWORD".to_owned(), "hunter2".to_owned());
    env.props.insert(
        "signing.properties".to_owned(),
        SIGNING_PROPERTIES.to_owned(),
    );

    let outcome = evaluate_project_with(CONVENTIONS, LIBS, BUILD, &env);
    assert!(
        outcome.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        outcome.diagnostics
    );

    let Value::Block(top) = &outcome.model else {
        panic!("expected a Block module model");
    };

    // A single `plugin "..."` stays a scalar; repeated scalar pairs would
    // accumulate into a List (see the module-level merge rule in eval.rs).
    assert_eq!(top["plugin"], Value::Str("android-application".to_owned()));

    // android {} merges the androidApp convention's compileSdk/minSdk/
    // targetSdk with the module's own namespace/applicationId/versionCode/
    // versionName into one block.
    let android = block(top, "android");
    assert_eq!(android["compileSdk"], Value::Number(Number::Int(37)));
    assert_eq!(android["minSdk"], Value::Number(Number::Int(24)));
    assert_eq!(android["targetSdk"], Value::Number(Number::Int(37)));
    assert_eq!(
        android["namespace"],
        Value::Str("com.example.app".to_owned())
    );
    assert_eq!(
        android["applicationId"],
        Value::Str("com.example.app".to_owned())
    );
    assert_eq!(android["versionCode"], Value::Number(Number::Int(7)));
    assert_eq!(
        android["versionName"],
        Value::Version(ulb_lang::eval::VersionValue {
            major: 0,
            minor: 1,
            patch: 2
        })
    );

    // buildTypes {} merges the convention's release with the fn helper's
    // debug and the module's own release (key-by-key).
    let build_types = block(top, "buildTypes");
    assert_eq!(
        build_types["debug"],
        Value::Block(BTreeMap::from([(
            "minifyEnabled".to_owned(),
            Value::Bool(false)
        )]))
    );
    let release = block(build_types, "release");
    assert_eq!(release["minifyEnabled"], Value::Bool(true));
    assert_eq!(
        release["proguardFiles"],
        Value::List(vec![Value::Str("proguard-rules.pro".to_owned())])
    );

    let flavors = block(top, "productFlavors");
    assert_eq!(flavors["dimension"], Value::Str("tier".to_owned()));
    assert_eq!(
        flavors["free"],
        Value::Block(BTreeMap::from([(
            "applicationIdSuffix".to_owned(),
            Value::Str(".free".to_owned())
        )]))
    );
    assert_eq!(
        flavors["paid"],
        Value::Block(BTreeMap::from([(
            "applicationIdSuffix".to_owned(),
            Value::Str(".paid".to_owned())
        )]))
    );

    // signing {} merges the env-signing convention's storeFile/storePassword
    // with the module's keyAlias/keyPassword; env()/props() resolve from the
    // injected environment.
    let signing = block(top, "signing");
    assert_eq!(
        signing["storeFile"],
        Value::Str("release.keystore".to_owned())
    );
    assert_eq!(signing["keyAlias"], Value::Str("sampleapp".to_owned()));
    assert_eq!(signing["storePassword"], Value::Str("hunter2".to_owned()));
    assert_eq!(signing["keyPassword"], Value::Str("hunter2".to_owned()));

    // deps {} accumulates repeated implementation pairs; a versioned alias
    // and a full-coordinate alias both resolve to a Coordinate.
    let deps = block(top, "deps");
    assert_eq!(
        coordinate_list(deps, "implementation"),
        vec![
            Value::Coordinate("androidx.core:core-ktx:1.15.0".to_owned()),
            Value::Coordinate("androidx.appcompat:appcompat:1.7.0".to_owned()),
        ]
    );

    // Dotted targets create nested blocks.
    let common_main = block(top, "commonMain");
    let common_deps = block(common_main, "deps");
    assert_eq!(
        common_deps["implementation"],
        Value::Coordinate("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0".to_owned())
    );
    let android_main = block(top, "androidMain");
    let android_deps = block(android_main, "deps");
    assert_eq!(
        android_deps["implementation"],
        Value::Coordinate("org.jetbrains.compose.ui:ui:1.8.0".to_owned())
    );

    // task {} captures its description, dependsOn list, and run {} actions
    // as data rather than executing them.
    let tasks = block(top, "tasks");
    let print_config = block(tasks, "printConfig");
    assert_eq!(
        print_config["description"],
        Value::Str("Prints the resolved module configuration.".to_owned())
    );
    assert_eq!(
        coordinate_list(print_config, "dependsOn"),
        vec![
            Value::Str("compileReleaseKotlin".to_owned()),
            Value::Str("bundleRelease".to_owned()),
        ]
    );
    let run = block(print_config, "run");
    let Value::List(actions) = &run["__actions__"] else {
        panic!("expected run to carry __actions__");
    };
    assert_eq!(actions.len(), 2);
    let Value::Block(exec) = &actions[0] else {
        panic!("expected exec action block");
    };
    assert_eq!(exec["action"], Value::Str("exec".to_owned()));
    assert_eq!(exec["command"], Value::Str("echo".to_owned()));
    assert_eq!(
        exec["args"],
        Value::List(vec![
            Value::Str("hello".to_owned()),
            Value::Str("from".to_owned()),
            Value::Str("ulb".to_owned()),
        ])
    );
    let Value::Block(copy) = &actions[1] else {
        panic!("expected copy action block");
    };
    assert_eq!(copy["action"], Value::Str("copy".to_owned()));
    assert_eq!(copy["from"], Value::Str("src/main/kotlin".to_owned()));
    assert_eq!(copy["to"], Value::Str("out/merged-kotlin".to_owned()));
}
