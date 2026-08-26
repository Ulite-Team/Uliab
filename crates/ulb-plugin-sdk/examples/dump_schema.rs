//! Prints the `ulb:config-schema` catalog embedded in a plugin artifact.
//!
//! Usage: `cargo run -p ulb-plugin-sdk --example dump_schema -- <plugin.wasm>`
//!
//! Reads the section statically — no wasmtime instantiation, no execution —
//! so CI can prove a built artifact carries its declared surface without
//! running it.

fn main() {
    let path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: dump_schema <plugin.wasm>");
            std::process::exit(2);
        }
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("error: reading {path}: {error}");
            std::process::exit(1);
        }
    };
    match ulb_plugin_sdk::schema::read_schema(&bytes) {
        Ok(Some(schema)) => {
            println!(
                "plugin schema ({} keys, {} features):",
                schema.keys.len(),
                schema.features.len()
            );
            for key in &schema.keys {
                let requirement = if key.required { "req" } else { "opt" };
                println!(
                    "k {}\t{}\t{}\t{}",
                    key.path,
                    key.kind.as_str(),
                    requirement,
                    key.description
                );
            }
            for (name, description) in &schema.features {
                println!("f {name}\t{description}");
            }
        }
        Ok(None) => {
            eprintln!("no ulb:config-schema section present");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
