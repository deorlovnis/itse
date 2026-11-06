//! `itse-fixture lsp`: a language server on stdio with `Content-Length` frames. It asks for its
//! `fixture` settings with `workspace/configuration`, indexes behind `$/progress` begin and end
//! (`index_ms` long), and publishes an error for each line holding `FIXTURE_ERROR` (or `marker`) in a file it is
//! told about (`didOpen`, `didChange`, `didSave`, from disk if unopened). Symbols, references and
//! hover text come from `ITSE_FIXTURE_SCRIPT`; a definition is the symbol on the asked line, else
//! the first:
//!
//! `{"encoding": "utf-16", "index_ms": 0, "slow_ms": 3000, "symbols": [{"name", "kind",
//! "container", "path", "line", "character"}], "references": [{"path", "line", "character"}],
//! "hover": "text", "check": false, "drop_saves": [2], "hierarchical": false, "links": false,
//! "caps_off": ["definitionProvider"], "definition": {"path", "line", "character"}, "marker": "FIXTURE_ERROR"}`
//!
//! With `check`, it publishes only on a save, behind `$/progress` begin and end, as rust-analyzer's
//! `cargo check` does, and ignores the saves `drop_saves` numbers (from 1). `hierarchical` answers
//! `documentSymbol` with `DocumentSymbol`s nested under the symbol their container names, `links`
//! a definition (`definition`, if given) with a `LocationLink`, and `caps_off` leaves capabilities out. It says on stderr
//! that it started.
//!
//! Paths are relative to the root, positions 0-based. Every message it gets is appended as one JSON
//! line to `ITSE_FIXTURE_LOG` when set. Modes: `crash-after-init` exits 3 after answering
//! `initialize`; `slow` answers it after `slow_ms`; `garbage-frame` sends a frame that isn't JSON
//! once initialized; `oversize-frame` answers `workspace/symbol` with 5 MiB; `spawn-child` starts a
//! `sleep` in a process group of its own once initialized, as rust-analyzer starts `cargo check`,
//! and writes its pid to the log's path plus `.child`. A `workspace/symbol` query for
//! `FIXTURE_CRASH` exits 3 mid-call.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::process::{self, Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};

use serde_json::{Value, json};

type Out = Arc<Mutex<io::Stdout>>;

fn send(out: &Out, v: Value) {
    let body = v.to_string();
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len()).and_then(|()| out.flush());
}

/// The next frame's body, or None at EOF.
fn frame(r: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((_, v)) = line.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")) {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    r.read_exact(&mut body).ok()?;
    Some(body)
}

/// An error for each line of `text` holding `marker`, at its column.
fn diagnostics(text: &str, marker: &str) -> Vec<Value> {
    let (lines, end) = (text.lines().enumerate().filter_map(|(n, l)| Some((n, l.find(marker)?))), marker.len());
    lines.map(|(n, c)| json!({"range": {"start": {"line": n, "character": c}, "end": {"line": n, "character": c + end}}, "severity": 1, "source": "fixture", "message": format!("{marker} found")})).collect()
}

/// The symbols whose container (or `module`, which `workspace/symbol` leaves out, as rust-analyzer
/// does for an inline module's items) is `parent`, each a `DocumentSymbol` with its own below it; a
/// symbol's range runs to the end of its line, a container's to the last line of what it holds.
fn nested(mine: &[&Value], parent: &Value) -> Vec<Value> {
    let pos = |s: &Value| json!({"line": s["line"], "character": s["character"]});
    let under = mine.iter().filter(|s| s.get("module").unwrap_or(&s["container"]) == parent);
    under
        .map(|s| {
            let children = nested(mine, &s["name"]);
            let last = children.iter().filter_map(|c| c["range"]["end"]["line"].as_u64()).max().unwrap_or(s["line"].as_u64().unwrap_or(0));
            let range = json!({"start": {"line": s["line"], "character": 0}, "end": {"line": last, "character": 200}});
            json!({"name": s["name"], "kind": s["kind"], "range": range, "selectionRange": {"start": pos(s), "end": pos(s)}, "children": children})
        })
        .collect()
}

fn location(root: &str, p: &Value) -> Value {
    let pos = json!({"line": p["line"], "character": p["character"]});
    json!({"uri": format!("{root}/{}", p["path"].as_str().unwrap_or("")), "range": {"start": pos, "end": pos}})
}

pub fn serve(mode: &str) -> ExitCode {
    let script: Value = env::var("ITSE_FIXTURE_SCRIPT").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    let log = env::var_os("ITSE_FIXTURE_LOG").and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok()).map(Mutex::new);
    let (out, mut stdin, mut root, mut saves) = (Arc::new(Mutex::new(io::stdout())), io::stdin().lock(), String::new(), 0);
    let (check, marker) = (script["check"] == true, script["marker"].as_str().unwrap_or("FIXTURE_ERROR").to_string());
    eprintln!("itse-fixture lsp: started");
    let publish = |out: &Out, uri: &str, text: &str| {
        send(out, json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": diagnostics(text, &marker)}}))
    };
    while let Some(body) = frame(&mut stdin) {
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        // One write per line: a test reading the log meanwhile never sees half of one.
        if let Some(log) = &log {
            let _ = log.lock().unwrap_or_else(|e| e.into_inner()).write_all(format!("{msg}\n").as_bytes());
        }
        let (id, p, method) = (msg["id"].clone(), &msg["params"], msg["method"].as_str().unwrap_or(""));
        let doc = &p["textDocument"];
        let uri = doc["uri"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => {
                root = p["rootUri"].as_str().unwrap_or("").to_string();
                if mode == "slow" {
                    thread::sleep(Duration::from_millis(script["slow_ms"].as_u64().unwrap_or(3000)));
                }
                let offered = p["capabilities"]["general"]["positionEncodings"].as_array().is_some_and(|e| e.iter().any(|e| e == "utf-8"));
                let encoding = script["encoding"].as_str().unwrap_or(if offered { "utf-8" } else { "utf-16" });
                let mut caps = json!({"positionEncoding": encoding, "textDocumentSync": {"openClose": true, "change": 1, "save": true}, "workspaceSymbolProvider": true,
                    "documentSymbolProvider": true, "definitionProvider": true, "referencesProvider": true, "hoverProvider": true});
                for off in script["caps_off"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    caps.as_object_mut().map(|c| c.remove(off));
                }
                json!({"capabilities": caps, "serverInfo": {"name": "itse-fixture"}})
            }
            "initialized" => {
                send(&out, json!({"jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration", "params": {"items": [{"section": "fixture"}]}}));
                send(&out, json!({"jsonrpc": "2.0", "id": "prog-1", "method": "window/workDoneProgress/create", "params": {"token": "fixture-index"}}));
                send(
                    &out,
                    json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "fixture-index", "value": {"kind": "begin", "title": "Indexing", "message": "0/3", "percentage": 0}}}),
                );
                send(&out, json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": "fixture indexing"}}));
                if let (Some(log), "spawn-child") = (env::var_os("ITSE_FIXTURE_LOG"), mode) {
                    let child = Command::new("sleep").arg("60").process_group(0).spawn().map(|c| c.id().to_string()).unwrap_or_default();
                    let _ = fs::write(format!("{}.child", log.to_string_lossy()), child);
                }
                if mode == "garbage-frame" {
                    let _ = write!(out.lock().unwrap_or_else(|e| e.into_inner()), "Content-Length: 5\r\n\r\nhello");
                }
                let (out, ms) = (out.clone(), script["index_ms"].as_u64().unwrap_or(0));
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(ms));
                    send(&out, json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "fixture-index", "value": {"kind": "end"}}}));
                });
                continue;
            }
            "textDocument/didOpen" if !check => {
                publish(&out, uri, doc["text"].as_str().unwrap_or(""));
                continue;
            }
            "textDocument/didChange" if !check => {
                publish(&out, uri, p["contentChanges"][0]["text"].as_str().unwrap_or(""));
                continue;
            }
            "textDocument/didSave" => {
                saves += 1;
                if script["drop_saves"].as_array().is_some_and(|d| d.contains(&json!(saves))) {
                    continue;
                }
                let progress = |kind: &str| json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "fixture-check", "value": {"kind": kind, "title": "Checking"}}});
                if check {
                    send(&out, progress("begin"));
                }
                publish(&out, uri, &fs::read_to_string(uri.strip_prefix("file://").unwrap_or(uri)).unwrap_or_default());
                if check {
                    send(&out, progress("end"));
                }
                continue;
            }
            "textDocument/didClose" => {
                publish(&out, uri, "");
                continue;
            }
            "exit" => return ExitCode::SUCCESS,
            "shutdown" => Value::Null,
            "workspace/symbol" if p["query"] == "FIXTURE_CRASH" => process::exit(3),
            "workspace/symbol" if mode == "oversize-frame" => json!([{"name": "x".repeat(5 << 20)}]),
            "workspace/symbol" => {
                let q = p["query"].as_str().unwrap_or("").to_lowercase();
                let found = script["symbols"].as_array().into_iter().flatten().filter(|s| s["name"].as_str().is_some_and(|n| n.to_lowercase().contains(&q)));
                Value::Array(found.map(|s| json!({"name": s["name"], "kind": s["kind"], "containerName": s["container"], "location": location(&root, s)})).collect())
            }
            "textDocument/documentSymbol" => {
                let mine: Vec<&Value> = script["symbols"].as_array().into_iter().flatten().filter(|s| location(&root, s)["uri"] == uri).collect();
                if script["hierarchical"] == true {
                    Value::Array(nested(&mine, &Value::Null))
                } else {
                    Value::Array(mine.iter().map(|s| json!({"name": s["name"], "kind": s["kind"], "containerName": s["container"], "location": location(&root, s)})).collect())
                }
            }
            "textDocument/references" => Value::Array(script["references"].as_array().into_iter().flatten().map(|r| location(&root, r)).collect()),
            "textDocument/definition" => {
                let symbols = script["symbols"].as_array().map_or(&[][..], Vec::as_slice);
                let here = symbols.iter().find(|s| location(&root, s)["uri"] == uri && s["line"] == p["position"]["line"]);
                let at = script.get("definition").or(here).or(symbols.first()).map(|s| location(&root, s));
                match at {
                    Some(l) if script["links"] == true => json!([{"targetUri": l["uri"], "targetRange": l["range"], "targetSelectionRange": l["range"]}]),
                    at => at.unwrap_or_default(),
                }
            }
            "textDocument/hover" => script["hover"].as_str().map_or(Value::Null, |h| json!({"contents": {"kind": "plaintext", "value": h}})),
            // A response to one of ours, or a notification it doesn't need.
            _ if id.is_null() || msg.get("method").is_none() => continue,
            other => {
                send(&out, json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {other}")}}));
                continue;
            }
        };
        send(&out, json!({"jsonrpc": "2.0", "id": id, "result": result}));
        if mode == "crash-after-init" && method == "initialize" {
            return ExitCode::from(3);
        }
    }
    ExitCode::SUCCESS
}
