//! Derive-macro behavior: catalog rendering, typed extraction, and the
//! error paths the generated `from_config` must honor.

use ulb_plugin_sdk::UlbConfig;

#[derive(UlbConfig, Debug)]
#[allow(dead_code)]
struct Flat {
    /// The greeting text.
    message: String,
}
