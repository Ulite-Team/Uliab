//! Derive-macro behavior: catalog rendering, typed extraction, and the
//! error paths the generated `from_config` must honor.

use ulb_plugin_sdk::UlbConfig;

#[derive(UlbConfig, Debug)]
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
