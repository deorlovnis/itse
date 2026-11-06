//! `itse-fixture mcp`: an MCP server on stdio with two tools. `echo` (read-only) answers its `text`; `slow`
//! answers after `ms` (default 30 s), on its own thread, so `ping` is answered meanwhile.
//!
//! For the hosted-server tests: `ITSE_FIXTURE_TOOLS=<n>` adds `n` tools with long descriptions and
//! one whose name is too long; `ITSE_FIXTURE_PAGE=<n>` pages the list with `nextCursor`;
//! `ITSE_FIXTURE_VERSION` is the version it answers (else the one asked); `ITSE_FIXTURE_INIT_MS`
//! delays its `initialize` answer; `ITSE_FIXTURE_LOG` gets every message it reads, one per line.
//! Modes: `crash-after-init`, `crash-in-call` (it exits 3 during a `slow` call), `grow` (the
//! first `echo` adds the tool `grown` and sends `notifications/tools/list_changed`), `ask` (once
//! initialized, it sends the client `ping`, sampling, elicitation and roots requests, whose
//! answers its log gets), `endless` (every page after the first is empty and names a next one)
//! and `bare` (a tool `bare` answers its `result` argument as the whole result, `{}` without
//! one). At the end of its stdin the log gets `{"eof": true}`.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::process::{self, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};

use serde_json::{Value, json};

fn tools(grown: bool, mode: &str) -> Vec<Value> {
    let schema = |key: &str, kind: &str| json!({"type": "object", "properties": {(key): {"type": kind}}});
    let mut tools = vec![
        json!({"name": "echo", "title": "Echo", "description": "Answers its text.", "inputSchema": schema("text", "string"), "annotations": {"readOnlyHint": true}}),
        json!({"name": "slow", "description": "Answers after ms milliseconds.", "inputSchema": schema("ms", "integer")}),
    ];
    let extra: usize = env::var("ITSE_FIXTURE_TOOLS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    let about = |i| {
        format!("Tool {i} answers its text. It has a description that goes on well past its first sentence, so that a client has something to clip: {}", "and more ".repeat(20))
    };
    tools.extend((1..=extra).map(|i| json!({"name": format!("tool-{i}"), "description": about(i), "inputSchema": schema("text", "string")})));
    if extra > 0 {
        tools.push(json!({"name": "a-tool-name-so-long-that-with-its-server-prefix-it-passes-64", "description": "Too long.", "inputSchema": schema("text", "string")}));
    }
    if grown {
        tools.push(json!({"name": "grown", "description": "Came later.", "inputSchema": schema("text", "string")}));
    }
    if mode == "bare" {
        tools.push(json!({"name": "bare", "description": "Answers its result.", "inputSchema": schema("result", "object")}));
    }
    tools
}

pub fn serve(mode: &str) -> ExitCode {
    let out = Arc::new(Mutex::new(io::stdout()));
    let send = |out: &Mutex<io::Stdout>, v: Value| {
        let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{v}").and_then(|()| out.flush());
    };
    let mut log = env::var("ITSE_FIXTURE_LOG").ok().and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    let page: usize = env::var("ITSE_FIXTURE_PAGE").ok().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
    let mut grown = false;
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{msg}");
        }
        let (id, params) = (msg["id"].clone(), &msg["params"]);
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => {
                thread::sleep(Duration::from_millis(env::var("ITSE_FIXTURE_INIT_MS").ok().and_then(|ms| ms.parse().ok()).unwrap_or(0)));
                let version = env::var("ITSE_FIXTURE_VERSION").unwrap_or_else(|_| params["protocolVersion"].as_str().unwrap_or("2025-11-25").into());
                json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": "itse-fixture", "version": env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "tools/list" if mode == "endless" && params["cursor"].is_string() => json!({"tools": [], "nextCursor": "again"}),
            "tools/list" => {
                let (all, from): (_, usize) = (tools(grown, mode), params["cursor"].as_str().and_then(|c| c.parse().ok()).unwrap_or(0));
                let end = all.len().min(from.saturating_add(page));
                let mut result = json!({"tools": all[from.min(end)..end]});
                if end < all.len() || mode == "endless" {
                    result["nextCursor"] = json!(end.to_string());
                }
                result
            }
            "tools/call" if params["name"] == "slow" && mode == "crash-in-call" => {
                thread::sleep(Duration::from_millis(200));
                process::exit(3);
            }
            "tools/call" if params["name"] == "slow" => {
                let (out, ms) = (out.clone(), params["arguments"]["ms"].as_u64().unwrap_or(30_000));
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(ms));
                    send(&out, json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("slept {ms} ms")}], "isError": false}}));
                });
                continue;
            }
            "tools/call" if params["name"] == "echo" || params["name"] == "grown" => {
                if mode == "grow" && !grown {
                    grown = true;
                    send(&out, json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
                }
                json!({"content": [{"type": "text", "text": params["arguments"]["text"]}], "isError": false})
            }
            "tools/call" if params["name"] == "bare" => params["arguments"].get("result").cloned().unwrap_or(json!({})),
            "tools/call" => json!({"content": [{"type": "text", "text": format!("no tool {}", params["name"])}], "isError": true}),
            "notifications/initialized" if mode == "ask" => {
                for (id, method) in [("p", "ping"), ("s", "sampling/createMessage"), ("e", "elicitation/create"), ("r", "roots/list")] {
                    send(&out, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}}));
                }
                continue;
            }
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
    if let Some(log) = log.as_mut() {
        let _ = writeln!(log, "{}", json!({"eof": true}));
    }
    ExitCode::SUCCESS
}
