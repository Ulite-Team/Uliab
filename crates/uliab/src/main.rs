//! The `uliab` command-line entry point.
//!
//! `uliab run <plugin.wasm> [input]` loads the component and prints the
//! result of its `run` entry point.

use std::path::Path;
use std::process::ExitCode;

use uliab::host::PluginHost;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 || args.first().map(String::as_str) != Some("run") {
        eprintln!("usage: uliab run <plugin.wasm> [input]");
        return ExitCode::from(2);
    }
    let path = Path::new(&args[1]);
    let input = args.get(2).map(String::as_str).unwrap_or("");

    let host = match PluginHost::new() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match host.run(path, input) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
