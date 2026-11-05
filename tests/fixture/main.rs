//! itse-fixture: fake servers and delegates for Itse's tests. Test code, built as a
//! second binary so e2e tests can spawn it; never installed, never in the `itse` binary.
//!
//! It speaks JSON with serde_json directly, not through Itse's modules, so Itse's codecs are never
//! tested against themselves. `ITSE_FIXTURE_MODE` drives the error paths (`exit:<n>`, `slow`,
//! `crash-after-init`). It has `mcp` and `tool` modes; `lsp` and `dap` are not built yet.

mod mcp;
mod tool;

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = env::var("ITSE_FIXTURE_MODE").unwrap_or_default();
    if let Some(code) = mode.strip_prefix("exit:") {
        return ExitCode::from(code.parse::<u8>().unwrap_or(1));
    }
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("{}", serde_json::json!({"fixture": env!("CARGO_PKG_VERSION")}));
            ExitCode::SUCCESS
        }
        Some("mcp") => mcp::serve(&mode),
        Some("tool") => tool::run(&args[1..], &mode),
        other => {
            eprintln!("itse-fixture: unknown mode '{}' (mcp and tool; lsp and dap are not built yet)", other.unwrap_or(""));
            ExitCode::from(2)
        }
    }
}
