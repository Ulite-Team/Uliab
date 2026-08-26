//! Derive-macro behavior: catalog rendering, typed extraction, and the
//! error paths the generated `from_config` must honor.

use ulb_plugin_sdk::UlbConfig;

#[derive(UlbConfig)]
#[allow(dead_code)]
struct Flat {
    /// The greeting text.
    message: String,
    /// How loud to be.
    volume: i32,
    /// Whether to shout.
    #[ulb(feature)]
    shout: bool,
    #[ulb(rename = "extraFiles")]
    extra_files: Vec<String>,
    /// A note nobody has to provide.
    note: Option<String>,
}

#[derive(UlbConfig)]
#[allow(dead_code)]
struct Nested {
    /// Nested depth.
    depth: i32,
}

#[derive(UlbConfig)]
#[allow(dead_code)]
struct Root {
    /// The nested block.
    inner: Nested,
}

#[test]
fn schema_catalog_renders_keys_features_and_descriptions() {
    let expected = [
        "ulb-config-schema 1",
        "k message\tstring\treq\tThe greeting text.",
        "k volume\tint\treq\tHow loud to be.",
        "k shout\tbool\treq\tWhether to shout.",
        "f shout\tWhether to shout.",
        "k extraFiles\tlist\treq\t",
        "k note\tstring\topt\tA note nobody has to provide.",
    ];
    assert_eq!(
        Flat::ULB_CONFIG_SCHEMA,
        expected.join("\n"),
        "{}",
        Flat::ULB_CONFIG_SCHEMA
    );
}

#[test]
fn from_config_reads_typed_values() {
    let value = serde_json::json!({
        "message": "hi",
        "volume": 3,
        "shout": true,
        "extraFiles": ["a.txt", "b.txt"],
        "note": null,
        "unknownKey": 42
    });
    let config = Flat::from_config(&value).expect("parses");
    assert_eq!(config.message, "hi");
    assert_eq!(config.volume, 3);
    assert!(config.shout);
    assert_eq!(config.extra_files, ["a.txt".to_owned(), "b.txt".to_owned()]);
    assert_eq!(config.note, None);
}

#[test]
fn missing_required_key_is_an_error_naming_the_key() {
    let error = Flat::from_config(&serde_json::json!({})).expect_err("must fail");
    assert_eq!(error, "missing required key 'message'");
}

#[test]
fn wrongly_typed_value_names_the_expected_kind() {
    let error = Flat::from_config(&serde_json::json!({ "message": 7 })).expect_err("must fail");
    assert_eq!(error, "key 'message' must be string");
}

#[test]
fn renamed_field_uses_the_dsl_name() {
    let error = Flat::from_config(&serde_json::json!({
        "message": "x", "volume": 1, "shout": false, "extraFilez": []
    }))
    .expect_err("renamed key is required under its DSL spelling");
    assert_eq!(error, "missing required key 'extraFiles'");
}

#[test]
fn nested_blocks_deserialize_through_their_own_derive() {
    let value = serde_json::json!({ "inner": { "depth": 9 } });
    let root = Root::from_config(&value).expect("parses");
    assert_eq!(root.inner.depth, 9);

    let error = Root::from_config(&serde_json::json!({ "inner": {} }))
        .expect_err("nested required key still enforced");
    assert_eq!(error, "missing required key 'depth'");

    // The parent's catalog declares the block entry only; the child's own
    // const carries its keys. Composition across structs is a later slice.
    assert!(
        Root::ULB_CONFIG_SCHEMA.contains("k inner\tblock\treq\tThe nested block."),
        "{}",
        Root::ULB_CONFIG_SCHEMA
    );
    assert!(
        Nested::ULB_CONFIG_SCHEMA.contains("k depth\tint\treq\tNested depth."),
        "{}",
        Nested::ULB_CONFIG_SCHEMA
    );
}
