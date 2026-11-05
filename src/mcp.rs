//! The MCP endpoint. `itse mcp` is a relay (`relay`) that carries a harness's
//! stdio to the workspace daemon, so warm servers outlive the harness; there `serve_io` answers MCP
//! on a connection whose `hello` said `mode: "mcp"` (Halti's mcp.rs loop).
//!
//! Three tools carry every verb an agent runs, with the CLI's own tokens in `args`, parsed by the
//! same function, so the skill, the index and MCP share one vocabulary; `debug` is listed only on
//! opt-in (`"mcp": {"debug": true}`). Claude Code defers schemas behind tool search, so the tool
//! names and `instructions` carry the keywords an agent searches for.

pub mod relay;

use std::collections::HashMap;
use std::io::{BufRead, ErrorKind, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

use crate::daemon::VERSION;
use crate::host::Host;
use crate::results::text;
use crate::rpc::{self, Cancel, Framing};
use crate::sys::lock;
use crate::usage::{Event, clip};
use crate::verbs::{self, Caller, Milestone, Tool, VERBS};

/// Echoed when a client asks for one of these; otherwise the newest is offered and the client decides.
pub const SUPPORTED: [&str; 2] = ["2025-11-25", "2025-06-18"];
/// The whole list, `instructions` included, stays under this many bytes.
pub const LIST_CAP: usize = 3600;
const DEBUG_VERBS: [&str; 7] = ["test", "attach", "step", "continue", "eval", "snapshot", "stop"];

pub fn tool_name(t: Tool) -> &'static str {
    match t {
        Tool::Run => "test_build_run",
        Tool::Query => "problems_symbols",
        Tool::Show => "show_result",
        Tool::Debug => "debug",
        Tool::CliOnly => "",
    }
}

/// A `next` command for the caller: a CLI line, or over MCP the tool call that runs it. A human's
/// step is the CLI line in a terminal for every caller.
pub fn next(tokens: &[String], via: &str, human: bool) -> String {
    match tokens.first().and_then(|v| verbs::find(v)) {
        Some(v) if via == "mcp" && !human && !v.human_only && !matches!(v.tool, Tool::CliOnly | Tool::Show) => {
            format!("{} {{verb: {:?}, args: {:?}}}", tool_name(v.tool), v.name, &tokens[1..])
        }
        _ if human => text("terminal", &[("cmd", &cli_line(tokens))]),
        _ => cli_line(tokens),
    }
}

/// The CLI line a shell reads back as these tokens: a plain token bare, any other single-quoted,
/// since the agent chose them and a human may paste the line.
fn cli_line(tokens: &[String]) -> String {
    let plain = |t: &String| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=@,+%".contains(&b));
    let quote = |t: &String| if plain(t) { t.clone() } else { format!("'{}'", t.replace('\'', r"'\''")) };
    ["itse".to_string()].into_iter().chain(tokens.iter().map(quote)).collect::<Vec<_>>().join(" ")
}

/// The verbs an agent can call through the core tools now: `stats` counts the others as never called.
pub fn offered() -> Vec<&'static str> {
    VERBS.iter().filter(|v| matches!(v.tool, Tool::Run | Tool::Query | Tool::Show) && v.milestone == Milestone::Built).map(|v| v.name).collect()
}

/// The tool list: three tools, and `debug` when opted in. Each verb enum comes from the verb table.
pub fn tools(debug: bool) -> Vec<Value> {
    let of = |t: Tool| VERBS.iter().filter(|v| v.tool == t && v.milestone == Milestone::Built).map(|v| v.name).collect::<Vec<_>>();
    let schema = |verbs: Vec<&str>, extra: Value| {
        let mut props = json!({"verb": {"type": "string", "enum": verbs}, "target": {"type": "string"}, "args": {"type": "array", "items": {"type": "string"}}});
        if let Some((p, e)) = props.as_object_mut().zip(extra.as_object()) {
            p.extend(e.clone());
        }
        json!({"type": "object", "properties": props, "required": ["verb"]})
    };
    let int = json!({"type": "integer"});
    let mut tools = vec![
        json!({"name": "test_build_run", "description": "Run tests, builds and declared actions; install, lock, use. args takes the CLI's tokens: [\"--action\", \"cargo:test\"]. Long runs return run:N for show_result.",
            "inputSchema": schema(of(Tool::Run), json!({"wait": int}))}),
        json!({"name": "problems_symbols", "description": "Problems (compile errors, diagnostics), symbols by name (find, refs, info, outline), status. args takes the CLI's tokens: [\"--new\"], [\"--kind\", \"method\"].",
            "inputSchema": schema(of(Tool::Query), json!({"page": int}))}),
        json!({"name": "show_result", "description": "Show a stored result or log by id (run:14, run:14/T1, q:7): pages, grep, tail.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "page": int, "grep": {"type": "string"}, "tail": int, "json": {"type": "boolean"}, "raw": {"type": "boolean"}}, "required": ["id"]}}),
    ];
    if debug {
        tools.push(json!({"name": "debug", "description": "Debug a test: stop, locals, stack, step, eval (DAP). args takes the CLI's tokens: [\"--at\", \"src/lib.rs:12\"].",
            "inputSchema": schema(DEBUG_VERBS.to_vec(), json!({}))}));
    }
    tools
}

/// What the 3,600-byte cap counts: the `tools/list` result as sent, plus `instructions`.
pub fn list_bytes(debug: bool) -> usize {
    json!({"tools": tools(debug)}).to_string().len() + text("instructions", &[]).len()
}

/// Serve MCP on a daemon connection after its `hello`, until EOF. A `tools/call` runs on its own
/// thread, so `ping` and `notifications/cancelled` are answered during a long test; one mutex
/// around the writer keeps lines whole.
pub fn serve_io(mut reader: impl BufRead, writer: &Mutex<impl Write + Send>, host: &Arc<Host>, caller: Caller) {
    let (calls, mut caller) = (Mutex::new(HashMap::<String, Cancel>::new()), Arc::new(caller));
    let send = |id: Value, reply: Result<Value, (i64, String)>| {
        let (key, value) = reply.map_or_else(|(code, message)| ("error", json!({"code": code, "message": message})), |r| ("result", r));
        let _ = rpc::write(&mut *lock(writer), Framing::Lines, &json!({"jsonrpc": "2.0", "id": id, (key): value}));
    };
    thread::scope(|scope| {
        loop {
            let mut msg = match rpc::read(&mut reader, Framing::Lines, rpc::MAX_FRAME) {
                Ok(Some(msg)) => msg,
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    send(Value::Null, Err((-32700, e.to_string())));
                    continue;
                }
                _ => break,
            };
            let (id, params, method) = (msg["id"].take(), msg["params"].take(), msg["method"].as_str().unwrap_or("").to_string());
            let reply = match method.as_str() {
                "initialize" => {
                    let (reply, next) = initialize(host, &caller, &params);
                    caller = Arc::new(next);
                    Ok(reply)
                }
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({"tools": tools(host.config().mcp.debug)})),
                "tools/call" => {
                    let (cancel, key, caller, send, calls) = (Cancel::default(), id.to_string(), caller.clone(), &send, &calls);
                    lock(calls).insert(key.clone(), cancel.clone());
                    scope.spawn(move || {
                        let result = tool_call(host, &caller, &params, &cancel);
                        lock(calls).remove(&key);
                        send(id, Ok(result));
                    });
                    continue;
                }
                "notifications/cancelled" => {
                    lock(&calls).get(&params["requestId"].to_string()).map(Cancel::cancel);
                    continue;
                }
                // Other notifications, and responses: we send no requests.
                _ if id.is_null() || method.is_empty() => continue,
                _ => Err((-32601, format!("method not found: {method}"))),
            };
            send(id, reply);
        }
        lock(&calls).values().for_each(Cancel::cancel);
    });
}

/// Agree on a version, take the client's name for the usage log, and record the session's start.
fn initialize(host: &Host, caller: &Caller, params: &Value) -> (Value, Caller) {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let proto = SUPPORTED.into_iter().find(|v| *v == asked).unwrap_or(SUPPORTED[0]);
    let mut next = caller.clone();
    if !caller.env.contains_key("ITSE_CLIENT") {
        next.id.client = clip(params["clientInfo"]["name"].as_str().unwrap_or("unknown"));
        next.id.client_v = params["clientInfo"]["version"].as_str().map(clip);
    }
    let debug = host.config().mcp.debug;
    let (client, client_v, tools, list_bytes) = (next.id.client.clone(), next.id.client_v.clone(), tools(debug).len() as u32, list_bytes(debug) as u64);
    host.usage.record(&caller.env, &caller.id.sess, Event::Init { client, client_v, proto: proto.into(), tools, list_bytes });
    let reply = json!({"protocolVersion": proto, "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": "itse", "version": VERSION}, "instructions": text("instructions", &[])});
    (reply, next)
}

/// A tool call as the `call` the CLI sends, answered as a tool result. `isError` is set only for
/// the error verdict; a human-only step is `NEEDS_HUMAN`, a state and not an error.
fn tool_call(host: &Arc<Host>, caller: &Caller, params: &Value, cancel: &Cancel) -> Value {
    let a = &params["arguments"];
    let token = |v: &Value| v.as_str().map_or_else(|| v.clone(), |s| s.into());
    let mut tokens: Vec<Value> = Vec::new();
    let verb = match params["name"].as_str().unwrap_or("") {
        "show_result" => {
            tokens.push(a["id"].clone());
            for k in ["page", "grep", "tail"].into_iter().filter(|k| !a[k].is_null()) {
                tokens.extend([format!("--{k}").into(), a[k].as_str().map_or_else(|| a[k].to_string(), String::from).into()]);
            }
            tokens.extend(["json", "raw"].into_iter().filter(|k| a[k] == true).map(|k| format!("--{k}").into()));
            "show".to_string()
        }
        "test_build_run" | "problems_symbols" => a["verb"].as_str().unwrap_or("").to_string(),
        "debug" if host.config().mcp.debug => {
            tokens.push(a["verb"].clone());
            "debug".to_string()
        }
        name => return json!({"content": [{"type": "text", "text": format!("ERROR {name} — no such tool\n")}], "isError": true}),
    };
    if params["name"] != "show_result" {
        tokens.extend(a.get("target").map(token));
        match &a["args"] {
            Value::Array(args) => tokens.extend(args.iter().map(token)),
            Value::Null => {}
            other => tokens.push(other.clone()),
        }
    }
    let reply = verbs::call(host, caller, &json!({"verb": verb, "tool": params["name"], "args": tokens, "page": a["page"], "wait": a["wait"]}), cancel);
    json!({"content": [{"type": "text", "text": reply["text"]}], "isError": matches!(reply["exit"].as_u64(), Some(2 | 130))})
}

#[cfg(test)]
mod tests;
