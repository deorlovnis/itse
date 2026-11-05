//! `itse-fixture mcp`: an MCP server on stdio with two tools. `echo` answers its `text`; `slow`
//! answers after `ms` (default 30 s), on its own thread, so `ping` is answered meanwhile.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

pub fn serve(mode: &str) -> ExitCode {
    let out = Arc::new(Mutex::new(io::stdout()));
    let send = |out: &Mutex<io::Stdout>, v: Value| {
        let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{v}").and_then(|()| out.flush());
    };
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let (id, params) = (msg["id"].clone(), &msg["params"]);
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => json!({"protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-11-25"), "capabilities": {"tools": {}},
                "serverInfo": {"name": "itse-fixture", "version": env!("CARGO_PKG_VERSION")}}),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [
                {"name": "echo", "description": "Answers its text.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                {"name": "slow", "description": "Answers after ms milliseconds.", "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}}]}),
            "tools/call" if params["name"] == "slow" => {
                let (out, ms) = (out.clone(), params["arguments"]["ms"].as_u64().unwrap_or(30_000));
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(ms));
                    send(&out, json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("slept {ms} ms")}], "isError": false}}));
                });
                continue;
            }
            "tools/call" if params["name"] == "echo" => json!({"content": [{"type": "text", "text": params["arguments"]["text"]}], "isError": false}),
            "tools/call" => json!({"content": [{"type": "text", "text": format!("no tool {}", params["name"])}], "isError": true}),
            _ if id.is_null() => continue,
            method => {
                send(&out, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}));
                continue;
            }
        };
        send(&out, json!({"jsonrpc": "2.0", "id": id, "result": result}));
        if mode == "crash-after-init" && msg["method"] == "initialize" {
            return ExitCode::from(3);
        }
    }
    ExitCode::SUCCESS
}
