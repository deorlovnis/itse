//! The DAP client end to end against `itse-fixture dap`: `debug test` in one call (the adapter
//! spawned on stdio, spawned and connected over TCP, or reached through an LSP server's command),
//! `debug attach`, the default stop rule, the snapshot and its caps, step, continue, eval and stop,
//! an adapter that ends, a killed daemon, trust and the usage log. Nothing in itse listens.

use std::net::TcpListener;

use super::*;

const LIB: &str = "// orders\npub fn save() {}\npub fn run() {}\npub fn more() {}\n";

/// A workspace that uses the extension `fix`: the fixture adapter reached by `via`, in this mode
/// and with this script, its messages logged; the fixture LSP server; and a test action whose one
/// failing test is at src/lib.rs:2. Trusted unless asked otherwise.
struct Dbg {
    env: Env,
    log: PathBuf,
    /// The extension's file, and the env every call gets on top of the test env.
    file: PathBuf,
    extra: Vec<(String, String)>,
}

impl Dbg {
    fn new(via: Value, mode: &str, script: Value) -> Self {
        Self::with(via, mode, script, true, &[])
    }

    fn with(via: Value, mode: &str, script: Value, trust: bool, extra: &[(&str, &str)]) -> Self {
        let env = Env::new();
        let root = env.work();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB).unwrap();
        let log = env.dir.join("fixture.log");
        let vars = json!({"ITSE_FIXTURE_MODE": mode, "ITSE_FIXTURE_LOG": log, "ITSE_FIXTURE_SCRIPT": script.to_string()});
        let dap = json!({"id": "fixture-dap", "protocol": "dap", "adapter_id": "fixture", "command": [FIXTURE, "dap"], "env": vars, "via": via, "hide_vars": ["^\\$i\\$a\\$"]});
        let lsp = json!({"id": "fixture", "protocol": "lsp", "command": [FIXTURE, "lsp"], "filetypes": ["rs"], "env": vars});
        let matcher = json!({"kind": "test", "pattern": {"regexp": "^FAILED (\\S+) at ([^:]+):(\\d+):(\\d+)$", "name": 1, "file": 2, "line": 3, "column": 4}});
        let test = json!({"label": "fix:test", "type": "process", "command": "/bin/sh", "args": ["-c", "echo 'FAILED orders::tests::saves at src/lib.rs:2:5'; exit 1"],
            "group": {"kind": "test", "isDefault": true}, "problemMatcher": matcher});
        let debug = json!({"adapter": "fixture-dap", "arguments": {"cwd": "${workspaceFolder}", "target": "${input:target}"}, "project_packages": ["orders"],
            "caught_condition": "package:${packages}"});
        let file = env.dir.join("fix.json");
        fs::write(&file, json!({"itse_ext": 1, "id": "fix", "servers": [dap, lsp], "actions": [test], "debug": debug}).to_string()).unwrap();
        let extra = extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let dbg = Self { env, log, file: file.clone(), extra };
        let steps: &[&[&str]] =
            if trust { &[&["ext", "add", file.to_str().unwrap()], &["use", "fix"], &["trust"]] } else { &[&["ext", "add", file.to_str().unwrap()], &["use", "fix"]] };
        for args in steps {
            let (code, text) = dbg.itse(args);
            assert_eq!(code, Some(0), "{args:?}: {text}");
        }
        dbg
    }

    /// `itse ARGS` in the work folder: (exit code, stdout and stderr).
    fn itse(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.env.command(&self.env.work(), args).envs(self.extra.iter().map(|(k, v)| (k, v))).output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    }

    /// The extension's file changed by `edit`, and added again.
    fn edit_ext(&self, edit: impl FnOnce(&mut Value)) -> (Option<i32>, String) {
        let mut ext: Value = serde_json::from_slice(&fs::read(&self.file).unwrap()).unwrap();
        edit(&mut ext);
        fs::write(&self.file, ext.to_string()).unwrap();
        self.itse(&["ext", "add", self.file.to_str().unwrap()])
    }

    /// The pid the fixture wrote for the child it started.
    fn child(&self) -> i32 {
        let path = self.log.with_extension("log.child");
        assert!(wait_until(Duration::from_secs(5), || fs::read_to_string(&path).is_ok_and(|p| !p.is_empty())));
        fs::read_to_string(&path).unwrap().trim().parse().unwrap()
    }

    /// `itse ARGS`, which must stop: its text.
    fn stopped(&self, args: &[&str]) -> String {
        let (code, text) = self.itse(args);
        assert!(code == Some(0) && text.starts_with("STOPPED debug "), "{args:?}: {text}");
        text
    }

    /// The DAP requests the fixture got, in order.
    fn requests(&self) -> Vec<Value> {
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|m| m["type"] == "request").collect()
    }

    fn commands(&self) -> Vec<String> {
        self.requests().iter().map(|m| m["command"].as_str().unwrap().to_string()).collect()
    }

    fn snapshot(&self, n: u32, k: u32) -> Value {
        serde_json::from_slice(&fs::read(self.env.daemons()[0].0.join(format!("debug/{n}/s{k}.json"))).unwrap()).unwrap()
    }

    /// The adapter's pid in procs.json, while it is recorded there.
    fn adapter_pid(&self, handle: &str) -> Option<i32> {
        let procs: Value = serde_json::from_slice(&fs::read(self.env.daemons().first()?.0.join("procs.json")).ok()?).ok()?;
        procs.as_array()?.iter().find(|p| p["handle"] == handle).and_then(|p| p["pid"].as_i64()).map(|p| p as i32)
    }
}

/// A free port on 127.0.0.1, for an adapter to listen on.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The inodes of the TCP sockets in LISTEN state, IPv4 and IPv6.
fn listening() -> Vec<String> {
    let rows = ["/proc/net/tcp", "/proc/net/tcp6"].into_iter().flat_map(|p| fs::read_to_string(p).unwrap_or_default().lines().skip(1).map(String::from).collect::<Vec<_>>());
    rows.filter_map(|r| {
        let f: Vec<&str> = r.split_whitespace().collect();
        (f.get(3) == Some(&"0A")).then(|| f.get(9).map(|s| s.to_string())).flatten()
    })
    .collect()
}

/// Whether process `pid` holds a listening TCP socket.
fn listens(pid: i32) -> bool {
    let socks: Vec<String> = fs::read_dir(format!("/proc/{pid}/fd")).unwrap().flatten().filter_map(|e| fs::read_link(e.path()).ok()).map(|l| l.display().to_string()).collect();
    listening().iter().any(|inode| socks.contains(&format!("socket:[{inode}]")))
}

#[test]
fn debug_test_stops_at_the_exception_in_one_call() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    let text = dbg.stopped(&["debug", "test"]);
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("STOPPED debug test — exception in orders::save, thread \"main\"; stop on: caught exceptions in orders; ") && first.ends_with("; dbg:1"), "{text}");
    let body = "\nsrc/lib.rs:2:5\n      1 | // orders\n  >   2 | pub fn save() {}\n      3 | pub fn run() {}\n      4 | pub fn more() {}\nlocals\n   order       T  Order(id=O-17)\n   this        T  OrderRepository@1\n      jdbc  T  JdbcTemplate@4f2a\n      c0  T  child\n";
    assert!(text.contains(body), "{text}");
    let tail = "\nexception AssertionError: expected 2, got 3\nframes (project)\n   F1 orders::save  src/lib.rs:2\n   F2 orders::tests::saves  src/lib.rs:1\n   … 3 framework frames\nmore: debug step|continue|eval|snapshot|stop dbg:1\n";
    assert!(
        text.ends_with(tail)
            && text.contains("\n      c8  T  child\n   long        T  xxx")
            && text.contains("\n   $exception  T  AssertionError\n      message  T  expected 2, got 3\n"),
        "{text}"
    );
    let r = dbg.requests();
    let init = &r[0]["arguments"];
    assert_eq!((init["clientID"].as_str(), init["adapterID"].as_str(), init["supportsRunInTerminalRequest"].as_bool()), (Some("itse"), Some("fixture"), Some(false)));
    assert_eq!(r[1]["arguments"], json!({"cwd": dbg.env.work(), "target": ""}), "the launch arguments, rendered");
    let order: Vec<String> = dbg.commands().into_iter().take(4).collect();
    assert_eq!(order, ["initialize", "launch", "setExceptionBreakpoints", "configurationDone"], "launch is sent before configurationDone");
    let exceptions = &r.iter().find(|m| m["command"] == "setExceptionBreakpoints").unwrap()["arguments"];
    assert_eq!(*exceptions, json!({"filters": ["uncaught"], "filterOptions": [{"filterId": "caught", "condition": "package:orders"}]}), "the declared condition");
    assert!(!text.contains("telemetry"), "telemetry is no output: {text}");
    let log = fs::read_to_string(&dbg.log).unwrap();
    assert!(log.contains(r#""command":"runInTerminal""#) && log.contains(r#""success":false"#), "runInTerminal is refused: {log}");
    let (code, shown) = dbg.itse(&["show", "dbg:1"]);
    assert_eq!((code, shown.lines().nth(1)), (Some(0), Some("src/lib.rs:2:5")), "the last snapshot stays showable: {shown}");
    let attaches: Vec<Value> = dbg.env.usage().into_iter().filter(|r| r["ev"] == "attach").collect();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].as_object().unwrap().keys().cloned().collect::<Vec<_>>(), ["ev", "outcome", "sess", "ts", "v", "ws"], "an attach holds no values");
    assert_eq!(attaches[0]["outcome"], "ok");
    assert!(dbg.itse(&["stats"]).1.contains("\ndebug attaches: 1 ok, 0 failed\n"));
    let pid = dbg.adapter_pid("dbg:1").expect("the adapter is in procs.json");
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:1"]), (Some(0), "STOPPED debug stop dbg:1 — stopped\n".into()));
    let bye = dbg.requests().into_iter().find(|m| m["command"] == "disconnect").expect("a disconnect");
    assert_eq!(bye["arguments"], json!({"terminateDebuggee": true}));
    assert!(wait_until(Duration::from_secs(3), || !alive(pid)) && dbg.adapter_pid("dbg:1").is_none(), "the adapter is gone");
}

#[test]
fn an_adapter_answering_launch_only_after_configuration_done_still_stops() {
    let dbg = Dbg::new(json!("program"), "java-order", json!({}));
    assert!(dbg.stopped(&["debug", "test"]).contains("\n   F1 orders::save  src/lib.rs:2\n"));
    assert_eq!(dbg.commands()[..4], ["initialize", "launch", "setExceptionBreakpoints", "configurationDone"]);
}

#[test]
fn debug_test_without_at_breaks_at_the_failing_tests_frame() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    let (code, text) = dbg.itse(&["test"]);
    assert!(code == Some(1) && text.contains("\nT1 failed src/lib.rs:2:5 orders::tests::saves\n"), "{text}");
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.starts_with("STOPPED debug test — breakpoint in orders::save, thread \"main\"; stop on: breakpoint at T1's frame src/lib.rs:2; "), "{text}");
    let bps = dbg.requests().into_iter().find(|m| m["command"] == "setBreakpoints").unwrap();
    assert_eq!(bps["arguments"], json!({"source": {"path": dbg.env.work().join("src/lib.rs")}, "breakpoints": [{"line": 2}]}));
    let named = dbg.stopped(&["debug", "test", "any", "run:1/T1"]);
    assert!(named.contains("; stop on: breakpoint at run:1/T1's frame src/lib.rs:2; "), "a named item: {named}");
    let alone = dbg.stopped(&["debug", "test", "run:1/T1"]);
    assert!(alone.contains("; stop on: breakpoint at run:1/T1's frame src/lib.rs:2; "), "the item alone: {alone}");
    let launches: Vec<Value> = dbg.requests().into_iter().filter(|m| m["command"] == "launch").map(|m| m["arguments"]["target"].clone()).collect();
    assert_eq!(launches, ["", "any", ""], "an item is no target");
    let at = dbg.stopped(&["debug", "test", "--at", "src/lib.rs:3", "--log", "src/lib.rs:4=order is {order}"]);
    assert!(at.contains("; stop on: breakpoints at src/lib.rs:3; ") && at.contains("\n  >   3 | pub fn run() {}\n"), "{at}");
    assert!(at.contains("\noutput: order is {order}\n"), "a logpoint prints and never stops: {at}");
    assert!(at.contains("; stop on: breakpoints at src/lib.rs:3; logpoints at src/lib.rs:4; dbg:"), "the logpoints are named: {at}");
    let both = dbg.stopped(&["debug", "test", "--log", "src/lib.rs:2=hi"]);
    assert!(both.contains("; stop on: breakpoint at T1's frame src/lib.rs:2 (in place of the logpoint there); dbg:"), "{both}");
    let bps = dbg.requests().into_iter().rfind(|m| m["command"] == "setBreakpoints").unwrap();
    assert_eq!(bps["arguments"]["breakpoints"], json!([{"line": 2}]), "one breakpoint on the line, not a logpoint too");
    assert_eq!(dbg.itse(&["debug", "test", "--log", "src/lib.rs:4"]).1, "ERROR debug test — 'src/lib.rs:4' is not path:line=text\n", "an empty logpoint would stop");
    assert_eq!(dbg.env.usage().last().unwrap()["err"], "bad_args");
}

#[test]
fn a_caught_only_adapter_skips_framework_stops() {
    let dbg = Dbg::new(json!("program"), "caught-only", json!({}));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.contains("; stop on: caught exceptions in orders (2 framework stops skipped); "), "{text}");
    let exceptions = dbg.requests().into_iter().find(|m| m["command"] == "setExceptionBreakpoints").unwrap();
    assert_eq!(exceptions["arguments"], json!({"filters": ["caught"]}), "only the plain filter is offered");
    assert_eq!(dbg.commands().iter().filter(|c| *c == "continue").count(), 2);
    assert!(text.contains("\n   F1 orders::save  src/lib.rs:2\n"), "{text}");
    let uncaught = Dbg::new(json!("program"), "caught-only", json!({"break_mode": "unhandled"}));
    let text = uncaught.stopped(&["debug", "test"]);
    assert!(text.contains("; stop on: caught exceptions in orders; dbg:1\n") && !uncaught.commands().contains(&"continue".to_string()), "an uncaught one is kept: {text}");
}

#[test]
fn a_scoped_caught_filter_keeps_its_stops_and_a_refused_condition_is_noted() {
    let dbg = Dbg::new(json!("program"), "framework-first", json!({}));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.starts_with("STOPPED debug test — exception in orders::save, thread \"main\"; stop on: caught exceptions in orders; dbg:1\n"), "{text}");
    assert!(!dbg.commands().contains(&"continue".to_string()), "the adapter scoped it: no stop is skipped");
    let refused = Dbg::new(json!("program"), "framework-first", json!({"reject_conditions": true}));
    let text = refused.stopped(&["debug", "test"]);
    let want = "; stop on: caught exceptions in orders (2 framework stops skipped; setExceptionBreakpoints: bad condition (0), so the plain caught filter, its stops outside the project skipped); dbg:1\n";
    assert!(text.contains(want) && text.contains("\n   F1 orders::save  src/lib.rs:2\n"), "{text}");
    let sets: Vec<Value> = refused.requests().into_iter().filter(|m| m["command"] == "setExceptionBreakpoints").map(|m| m["arguments"].clone()).collect();
    assert_eq!(sets[1], json!({"filters": ["caught", "uncaught"]}), "retried with the plain filter");
}

#[test]
fn snapshots_cap_variables_children_depth_and_long_values() {
    let dbg = Dbg::new(json!("program"), "", json!({"locals": 100}));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.chars().count() < 4300 && text.contains("\n   v15         T  15\nexception "), "20 locals shown: {text}");
    let snap = dbg.snapshot(1, 1);
    let vars = snap["frames"][0]["scopes"][0]["vars"].as_array().unwrap();
    assert_eq!(vars.len(), 20, "20 of 104 locals");
    assert_eq!(snap["frames"][0]["scopes"].as_array().unwrap().len(), 1, "the expensive scope is skipped");
    let named = |n: &str| vars.iter().find(|v| v["name"] == n).unwrap();
    assert_eq!(named("long")["value"].as_str().unwrap().chars().count(), 160);
    assert_eq!(named("this")["children"].as_array().unwrap().len(), 10, "10 children of this");
    assert_eq!(named("this")["children"][0]["children"], json!([]), "this at depth 2, no deeper");
    assert_eq!((named("order")["children"].clone(), named("$exception")["children"][0]["name"].clone()), (json!([]), json!("message")), "depth 1, and 2 for the exception");
    let asked: Vec<i64> = dbg.requests().iter().filter(|m| m["command"] == "variables").map(|m| m["arguments"]["variablesReference"].as_i64().unwrap()).collect();
    assert!(!asked.contains(&9) && !asked.contains(&2000) && !asked.contains(&3001), "{asked:?}");
    assert_eq!(snap["frames"].as_array().unwrap().len(), 2, "project frames kept");
    assert_eq!(snap["framework"], 3);
    assert!(dbg.requests().len() <= 50 + 8, "the snapshot's 50 requests and the handshake's");
}

#[test]
fn huge_names_are_clipped_and_a_long_snapshot_says_it_is_truncated() {
    let dbg = Dbg::new(json!("program"), "", json!({"locals": 30, "long_names": 70_000}));
    let text = dbg.stopped(&["debug", "test"]);
    let first = text.lines().next().unwrap();
    assert!(first.chars().count() < 500 && first.contains(" in orders::save___") && first.contains("…, thread \"main___"), "{first}");
    assert!(text.chars().count() < 4600, "{}", text.chars().count());
    let path = dbg.env.daemons()[0].0.join("debug/1/s1.json");
    assert!(text.contains(&format!("\ntruncated; the whole snapshot: {}\nmore: ", path.display())), "{text}");
    assert!(fs::metadata(&path).unwrap().len() < 100_000, "the stored snapshot is clipped too");
    let wide = Dbg::new(json!("program"), "", json!({"locals": 100, "long_names": 300}));
    let text = wide.stopped(&["debug", "test"]);
    let kept = ["\nexception AssertionError: expected 2, got 3\nframes (project)\n   F1 orders::save", " more locals\nexception ", "\n   … 3 framework frames\n"];
    assert!(kept.iter().all(|k| text.contains(k)), "fewer locals, never the exception or the frames: {text}");
}

#[test]
fn hide_vars_never_reach_a_snapshot() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    let text = dbg.stopped(&["debug", "test"]);
    let snap = dbg.snapshot(1, 1).to_string();
    for t in [&text, &snap] {
        assert!(!t.contains("$i$a$secret") && !t.contains("hidden-value"), "{t}");
    }
}

#[test]
fn step_continue_eval_snapshot_and_stop_work_on_the_fixture() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    dbg.stopped(&["debug", "test", "--at", "src/lib.rs:2"]);
    let text = dbg.stopped(&["debug", "step", "dbg:1"]);
    assert!(text.starts_with("STOPPED debug step dbg:1 — after a step in orders::save, thread \"main\"; dbg:1\nsrc/lib.rs:3:5\n"), "{text}");
    assert!(dbg.stopped(&["debug", "step", "dbg:1", "in"]).contains("\nsrc/lib.rs:4:5\n"));
    let steps: Vec<Value> = dbg.requests().into_iter().filter(|m| matches!(m["command"].as_str(), Some("next" | "stepIn"))).map(|m| m["arguments"].clone()).collect();
    assert_eq!(steps, [json!({"threadId": 1, "granularity": "line"}), json!({"threadId": 1, "granularity": "line"})]);
    assert_eq!(dbg.itse(&["debug", "eval", "dbg:1", "order.id"]), (Some(0), "OK debug eval dbg:1 order.id — order.id = 42 in frame 2 (Int)\n".into()));
    assert_eq!(dbg.itse(&["debug", "eval", "dbg:1", "x", "--frame", "F2"]).1, "OK debug eval dbg:1 x — x = 42 in frame 3 (Int)\n");
    let other = dbg.stopped(&["debug", "snapshot", "dbg:1", "--frame", "F2"]);
    assert!(other.starts_with("STOPPED debug snapshot dbg:1 — after a step in F2 orders::tests::saves, ") && other.contains("\nsrc/lib.rs:1:5\n  >   1 | // orders\n"), "{other}");
    assert_eq!(dbg.itse(&["debug", "step", "dbg:1", "sideways"]).1, "ERROR debug step dbg:1 sideways — 'debug step' takes over, in or out, not 'sideways'\n");
    assert_eq!(dbg.env.usage().last().unwrap()["err"], "bad_args");
    assert_eq!(dbg.itse(&["debug", "snapshot", "dbg:1", "--frame", "F9"]).1, "ERROR debug snapshot dbg:1 — the stop has 2 project frames, so no F9\n");
    assert_eq!(dbg.itse(&["debug", "eval", "dbg:1", "x", "--frame", "F3"]).1, "ERROR debug eval dbg:1 x — the stop has 2 project frames, so no F3\n");
    let ended = "EXITED debug continue dbg:1 — the debuggee ended with exit code 0; output: test finished\n";
    assert_eq!(dbg.itse(&["debug", "continue", "dbg:1"]), (Some(0), ended.into()));
    assert_eq!(dbg.itse(&["debug", "snapshot", "dbg:1"]), (Some(2), "ERROR debug snapshot dbg:1 — dbg:1 has ended; its last snapshot: itse show dbg:1\n".into()));
    let (code, shown) = dbg.itse(&["show", "dbg:1"]);
    assert!(code == Some(0) && shown.starts_with("EXITED debug snapshot dbg:1 — dbg:1 has ended; its last stop: after a step in F2 ") && !shown.contains("more: "), "{shown}");
    let again = "OK debug stop dbg:1 — nothing to stop: dbg:1 has ended; its last snapshot: itse show dbg:1\n";
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:1"]), (Some(0), again.into()), "a stop after the end is no error");
    for (args, missing) in [(&["debug", "step"][..], "<dbg>"), (&["debug", "stop"], "<dbg>"), (&["debug", "attach"], "<host:port>")] {
        assert_eq!(dbg.itse(args), (Some(2), format!("ERROR {} — missing argument {missing}\n", args.join(" "))));
    }
    dbg.stopped(&["debug", "test"]);
    assert_eq!(dbg.itse(&["stop", "dbg:2"]), (Some(0), "STOPPED stop dbg:2 — stopped\n".into()), "the plain stop verb stops a session too");
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:9"]).1, "ERROR debug stop dbg:9 — dbg:9 is not running\n");
}

#[test]
fn an_adapter_that_ends_leaves_its_session_gone_with_its_last_snapshot() {
    let dbg = Dbg::new(json!("program"), "eof-on-continue", json!({}));
    dbg.stopped(&["debug", "test"]);
    let (code, text) = dbg.itse(&["debug", "continue", "dbg:1"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug continue dbg:1 — dbg:1 is gone: the adapter ended (eof); its last snapshot: itse show dbg:1\n"));
    let (code, text) = dbg.itse(&["debug", "snapshot", "dbg:1"]);
    assert!(
        code == Some(2) && text.starts_with("ERROR debug snapshot dbg:1 — dbg:1 is gone: the adapter ended") && text.ends_with("; its last snapshot: itse show dbg:1\n"),
        "{text}"
    );
    assert_eq!(dbg.itse(&["show", "dbg:1"]).0, Some(0));
    let errs: Vec<Value> = dbg.env.usage().into_iter().filter(|r| r["verb"] == "debug" && r["outcome"] == "error").map(|r| r["err"].clone()).collect();
    assert_eq!(errs, ["io", "unknown_id"], "the adapter ending mid-call is no bad id; a later call names one");
    let mid = Dbg::new(json!("program"), "eof-on-scopes", json!({}));
    let (code, text) = mid.itse(&["debug", "test"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug test — dbg:1 is gone: the adapter ended (eof)\n"), "no snapshot that looks whole");
    assert_eq!(mid.env.usage().last().unwrap()["err"], "io");
}

#[test]
fn an_adapter_refusing_the_launch_says_why_at_once() {
    let dbg = Dbg::new(json!("program"), "launch-fails", json!({}));
    let started = Instant::now();
    let (code, text) = dbg.itse(&["debug", "test"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug test — launch: program not found: nope (0)\n"));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

#[test]
fn ctrl_c_cancels_a_debug_test_waiting_for_its_launch() {
    let dbg = Dbg::new(json!("program"), "java-order", json!({"hold_launch": true}));
    let cli = dbg.env.command(&dbg.env.work(), &["debug", "test"]).stdout(Stdio::piped()).spawn().unwrap();
    assert!(wait_until(Duration::from_secs(5), || dbg.commands().contains(&"configurationDone".to_string())));
    let started = Instant::now();
    signal(cli.id() as i32, "INT");
    let out = cli.wait_with_output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!((out.status.code(), stdout(&out)), (Some(130), "ERROR debug test — cancelled\n".to_string()));
}

#[test]
fn a_stop_without_a_thread_id_is_the_first_threads() {
    let dbg = Dbg::new(json!("program"), "no-thread-id", json!({}));
    assert!(dbg.stopped(&["debug", "test"]).contains("\n   F1 orders::save  src/lib.rs:2\n"));
    assert!(dbg.stopped(&["debug", "step", "dbg:1"]).contains("\nsrc/lib.rs:3:5\n"));
    let threads: Vec<Value> =
        dbg.requests().into_iter().filter(|m| matches!(m["command"].as_str(), Some("stackTrace" | "next"))).map(|m| m["arguments"]["threadId"].clone()).collect();
    assert!(threads.iter().all(|t| *t == 7), "{threads:?}");
}

#[test]
fn what_the_adapter_starts_in_a_group_of_its_own_ends_with_the_session() {
    let dbg = Dbg::new(json!("program"), "spawn-child", json!({}));
    dbg.stopped(&["debug", "test"]);
    let child = dbg.child();
    assert!(alive(child));
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:1"]).0, Some(0));
    assert!(wait_until(Duration::from_secs(3), || !alive(child)), "the marked child goes too");
}

#[test]
fn an_idle_session_is_reaped() {
    let dbg = Dbg::with(json!("program"), "", json!({}), true, &[("ITSE_DEBUG_IDLE_S", "1"), ("ITSE_DAEMON_IDLE_S", "30")]);
    dbg.stopped(&["debug", "test"]);
    let pid = dbg.adapter_pid("dbg:1").unwrap();
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)), "the adapter is stopped");
    let (code, text) = dbg.itse(&["debug", "step", "dbg:1"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug step dbg:1 — dbg:1 is gone: idle for 1 s; its last snapshot: itse show dbg:1\n"));
}

#[test]
fn a_killed_daemon_leaves_the_session_gone_and_its_adapter_killed() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    dbg.stopped(&["debug", "test"]);
    let (adapter, daemon) = (dbg.adapter_pid("dbg:1").unwrap(), dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32);
    signal(daemon, "KILL");
    assert!(wait_until(Duration::from_secs(3), || !alive(daemon)));
    let (code, text) = dbg.itse(&["debug", "snapshot", "dbg:1"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug snapshot dbg:1 — dbg:1 is gone: daemon restarted; its last snapshot: itse show dbg:1\n"));
    assert!(wait_until(Duration::from_secs(3), || !alive(adapter)), "the new daemon killed the old adapter");
    assert!(dbg.itse(&["show", "dbg:1"]).1.contains("\nsrc/lib.rs:2:5\n"));
    let port = free_port();
    let mut adapter = Command::new(FIXTURE).args(["dap", "--tcp", &port.to_string()]).current_dir(dbg.env.work()).stderr(Stdio::null()).spawn().unwrap();
    dbg.stopped(&["debug", "attach", &format!("127.0.0.1:{port}"), "--at", "src/lib.rs:3"]);
    signal(dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32, "KILL");
    let (code, text) = dbg.itse(&["debug", "step", "dbg:2"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug step dbg:2 — dbg:2 is gone: daemon restarted; its last snapshot: itse show dbg:2\n"), "an attach too");
    let _ = adapter.kill();
    let _ = adapter.wait();
}

#[test]
fn tcp_mode_spawns_the_adapter_then_connects_and_attach_connects_to_one() {
    let port = free_port();
    let via = json!({"tcp": {"argv": [FIXTURE, "dap", "--tcp", port.to_string()], "port": port}});
    let dbg = Dbg::new(via, "", json!({}));
    assert!(dbg.stopped(&["debug", "test"]).contains("\n   F1 orders::save  src/lib.rs:2\n"));
    let daemon = dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    assert!(!listens(daemon), "itse connects, never listens");
    let (code, text) = dbg.itse(&["debug", "test"]);
    let taken = format!("ERROR debug test — port {port} is taken already (by another debug session?), so fixture-dap can't listen there\n");
    assert_eq!((code, text), (Some(2), taken), "never a second session on dbg:1's adapter");
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:1"]).0, Some(0));
    let port = free_port();
    let mut adapter =
        Command::new(FIXTURE).args(["dap", "--tcp", &port.to_string()]).current_dir(dbg.env.work()).env("ITSE_FIXTURE_LOG", &dbg.log).stderr(Stdio::null()).spawn().unwrap();
    let text = dbg.stopped(&["debug", "attach", &format!("127.0.0.1:{port}"), "--at", "src/lib.rs:3"]);
    assert!(text.contains("; stop on: breakpoints at src/lib.rs:3; dbg:3\nsrc/lib.rs:3:5\n"), "{text}");
    let attach = dbg.requests().into_iter().find(|m| m["command"] == "attach").unwrap();
    assert_eq!(attach["arguments"], json!({}), "no attach block: an empty arguments object, never null");
    assert!(!text.contains("$i$a$secret"), "the adapter's hide_vars hold for an attach: {text}");
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:3"]).0, Some(0));
    let bye = dbg.requests().into_iter().rfind(|m| m["command"] == "disconnect").unwrap();
    assert_eq!(bye["arguments"], json!({"terminateDebuggee": false}), "an attached debuggee goes on");
    let _ = adapter.kill();
    let _ = adapter.wait();
}

#[test]
fn a_non_loopback_address_is_refused_and_counted_as_a_failed_attach() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    for (addr, host) in [("10.0.0.1:5005", "10.0.0.1"), ("localhost:5005", "localhost"), ("[::]:5005", "[::]")] {
        let (code, text) = dbg.itse(&["debug", "attach", addr]);
        assert_eq!((code, text), (Some(2), format!("ERROR debug attach {addr} — {host} is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only\n")));
    }
    let failed = dbg.env.usage().into_iter().filter(|r| r["ev"] == "attach" && r["outcome"] == "fail").count();
    assert_eq!(failed, 3);
    assert!(!fs::exists(&dbg.log).unwrap(), "nothing was started");
}

#[test]
fn an_attach_checks_its_address_before_the_template_and_connects_only_once_it_renders() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    assert_eq!(dbg.edit_ext(|e| e["debug"]["attach"] = json!({"x": "${cache}"})).0, Some(0));
    assert_eq!(dbg.itse(&["trust"]).0, Some(0));
    let (code, text) = dbg.itse(&["debug", "attach", "10.0.0.1:5005"]);
    let refused = "ERROR debug attach 10.0.0.1:5005 — 10.0.0.1 is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only\n";
    assert_eq!((code, text.as_str()), (Some(2), refused), "the loopback refusal comes before the template's error");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (code, text) = dbg.itse(&["debug", "attach", &addr]);
    assert_eq!((code, text), (Some(2), format!("ERROR debug attach {addr} — nothing fills ${{cache}} in '${{cache}}'\n")));
    assert!(listener.accept().is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock), "no connection to the debuggee for a template that can't render");
}

#[test]
fn lsp_command_mode_connects_to_the_returned_port() {
    let via = json!({"lsp_command": {"server": "fixture", "command": "start_debug_server"}});
    let dbg = Dbg::new(via, "", json!({"debug_port_as": "string"}));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.contains("\n   F1 orders::save  src/lib.rs:2\n"), "{text}");
    let log = fs::read_to_string(&dbg.log).unwrap();
    assert!(log.contains(r#""method":"workspace/executeCommand""#) && log.contains(r#""command":"start_debug_server""#), "{log}");
    assert!(!listens(dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32), "itse connects, never listens");
    thread::sleep(Duration::from_secs(4));
    assert!(dbg.stopped(&["debug", "step", "dbg:1"]).contains("\nsrc/lib.rs:3:5\n"), "the session holds its server past ITSE_SERVER_IDLE_S=2");
    assert_eq!(dbg.edit_ext(|e| e["servers"][1]["env"]["X"] = json!("1")).0, Some(0));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.contains(" (config changed; 'itse stop lsp:fixture' to apply); dbg:2\n"), "the warm server's old config is noted: {text}");
    let away = Dbg::new(json!({"lsp_command": {"server": "fixture", "command": "start_debug_server"}}), "", json!({"debug_port_as": "10.0.0.1"}));
    let (code, text) = away.itse(&["debug", "test"]);
    let why = "— lsp:fixture's start_debug_server answered \"10.0.0.1:";
    assert!(code == Some(2) && text.contains(why) && text.ends_with(": 10.0.0.1 is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only\n"), "{text}");
}

#[test]
fn untrusted_debug_returns_untrusted_and_starts_nothing() {
    let dbg = Dbg::with(json!("program"), "", json!({}), false, &[]);
    let (code, text) = dbg.itse(&["debug", "test"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED debug test — a debug launch runs project code\nuntrusted      workspace "), "{text}");
    assert!(text.trim_end().ends_with("next (human): itse trust in a terminal"), "{text}");
    let port = free_port();
    let mut adapter =
        Command::new(FIXTURE).args(["dap", "--tcp", &port.to_string()]).current_dir(dbg.env.work()).env("ITSE_FIXTURE_LOG", &dbg.log).stderr(Stdio::null()).spawn().unwrap();
    let (code, text) = dbg.itse(&["debug", "attach", &format!("127.0.0.1:{port}")]);
    assert!(code == Some(3) && text.starts_with(&format!("UNTRUSTED debug attach 127.0.0.1:{port} — a debug attach runs project code\n")), "{text}");
    let _ = adapter.kill();
    let _ = adapter.wait();
    assert!(!fs::exists(&dbg.log).unwrap(), "no adapter started, none connected to");
    assert_eq!(dbg.itse(&["trust"]).0, Some(0));
    dbg.stopped(&["debug", "test", "--at", "src/lib.rs:2"]);
    assert_eq!(dbg.edit_ext(|e| e["debug"]["arguments"]["program"] = json!("/bin/other")).0, Some(0));
    let (code, text) = dbg.itse(&["debug", "eval", "dbg:1", "order"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug eval dbg:1 order — dbg:1 is stopped: the workspace is not trusted now; its last snapshot: itse show dbg:1\n"));
    assert_eq!(dbg.itse(&["debug", "test"]).0, Some(3), "another launch asks for trust again");
    assert_eq!(dbg.itse(&["trust"]).0, Some(0));
    assert_eq!(dbg.edit_ext(|e| e["servers"][0]["via"] = json!({"tcp": {"argv": ["/bin/other"], "port": 5005}})).0, Some(0));
    assert_eq!(dbg.itse(&["debug", "test"]).0, Some(3), "and so does another adapter command in via");
    assert_eq!(dbg.itse(&["trust"]).0, Some(0));
    assert_eq!(dbg.edit_ext(|e| e["debug"]["project_packages"] = json!(["billing"])).0, Some(0));
    assert_eq!(dbg.itse(&["debug", "test"]).0, Some(2), "the stop rule runs nothing: no trust asked, so /bin/other is started");
    let (code, text) = dbg.edit_ext(|e| e["servers"][0]["hide_vars"] = json!(["*secret*"]));
    assert!(code == Some(2) && text.contains("fixture-dap hide_vars: regex '*secret*' is refused"), "a pattern that hides nothing is refused: {text}");
}

#[test]
fn a_live_debug_session_survives_a_replaced_binary() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    dbg.stopped(&["debug", "test", "--at", "src/lib.rs:2"]);
    let daemon = dbg.env.daemon_json()["pid"].as_i64().unwrap();
    let out = dbg.env.copy_status();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(dbg.env.daemon_json()["pid"].as_i64(), Some(daemon), "a live session keeps its daemon serving");
    assert!(dbg.stopped(&["debug", "step", "dbg:1"]).contains("\nsrc/lib.rs:3:5\n"));
}

#[test]
fn a_daemon_that_stops_disconnects_its_sessions_first() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    let port = free_port();
    let mut adapter =
        Command::new(FIXTURE).args(["dap", "--tcp", &port.to_string()]).current_dir(dbg.env.work()).env("ITSE_FIXTURE_LOG", &dbg.log).stderr(Stdio::null()).spawn().unwrap();
    dbg.stopped(&["debug", "attach", &format!("127.0.0.1:{port}"), "--at", "src/lib.rs:3"]);
    let daemon = dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    signal(daemon, "TERM");
    assert!(wait_until(Duration::from_secs(5), || !alive(daemon)));
    let bye = dbg.requests().into_iter().find(|m| m["command"] == "disconnect").expect("a disconnect before the daemon's exit");
    assert_eq!(bye["arguments"], json!({"terminateDebuggee": false}), "the attached debuggee goes on");
    let (code, text) = dbg.itse(&["debug", "step", "dbg:1"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR debug step dbg:1 — dbg:1 is gone: the daemon stopped; its last snapshot: itse show dbg:1\n"));
    let _ = adapter.kill();
    let _ = adapter.wait();
}

#[test]
fn a_session_that_ended_or_never_started_says_so_after_a_restart() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    assert!(dbg.itse(&["debug", "attach", "127.0.0.1:1"]).1.starts_with("ERROR debug attach 127.0.0.1:1 — connecting to 127.0.0.1 port 1"));
    assert_eq!(dbg.itse(&["debug", "snapshot", "dbg:1"]).1, "ERROR debug snapshot dbg:1 — dbg:1 is not running: its start failed\n");
    dbg.stopped(&["debug", "test"]);
    assert_eq!(dbg.itse(&["debug", "continue", "dbg:2"]).0, Some(0));
    dbg.stopped(&["debug", "test"]);
    assert_eq!(dbg.itse(&["debug", "stop", "dbg:3"]).0, Some(0));
    let daemon = dbg.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    signal(daemon, "TERM");
    assert!(wait_until(Duration::from_secs(5), || !alive(daemon)));
    assert_eq!(dbg.itse(&["debug", "step", "dbg:2"]).1, "ERROR debug step dbg:2 — dbg:2 has ended; its last snapshot: itse show dbg:2\n");
    assert_eq!(dbg.itse(&["debug", "step", "dbg:3"]).1, "ERROR debug step dbg:3 — dbg:3 is stopped; its last snapshot: itse show dbg:3\n");
}

#[test]
fn unverified_breakpoints_missing_sources_and_dropped_logpoints_are_noted() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    assert_eq!(dbg.itse(&["debug", "test", "--at", "nosuch.rs:3"]), (Some(2), "ERROR debug test — nosuch.rs: no such file\n".into()));
    assert!(!fs::exists(&dbg.log).unwrap(), "refused before anything starts");
    let text = dbg.stopped(&["debug", "test", "--at", "src/lib.rs:99", "--at", "src/lib.rs:3"]);
    let rule = "stop on: breakpoints at src/lib.rs:99, src/lib.rs:3 (src/lib.rs:99 unverified (no code at this line)); dbg:1\n";
    assert!(text.contains(rule) && text.contains("\n  >   3 | pub fn run() {}\n"), "{text}");
    let (code, text) = dbg.itse(&["debug", "test", "--at", "src/lib.rs:99"]);
    assert!(code == Some(1) && text.starts_with("EXITED debug test — no stop (stop on: breakpoints at src/lib.rs:99 (src/lib.rs:99 unverified"), "no stop is a failure: {text}");
    let out = dbg.env.command(&dbg.env.work().join("src"), &["debug", "test", "--at", "lib.rs:3"]).output().unwrap();
    assert!(stdout(&out).contains("; stop on: breakpoints at src/lib.rs:3; "), "a path from the caller's folder: {}", stdout(&out));
    let caught = Dbg::new(json!("program"), "caught-only", json!({}));
    let text = caught.stopped(&["debug", "test", "--log", "src/lib.rs:4=x"]);
    assert!(text.contains("; stop on: caught exceptions in orders (2 framework stops skipped; src/lib.rs: logpoints dropped, as the adapter has none); "), "{text}");
}

#[test]
fn a_failing_exception_info_decides_nothing_and_ends_nothing() {
    let dbg = Dbg::new(json!("program"), "caught-only", json!({"fail_exception_info": true}));
    let text = dbg.stopped(&["debug", "test"]);
    assert!(text.contains("; stop on: caught exceptions in orders (2 framework stops skipped); ") && text.contains("\n   F1 orders::save  src/lib.rs:2\n"), "{text}");
}

#[test]
fn the_verb_index_lists_debug_where_a_debug_adapter_is_declared() {
    let dbg = Dbg::new(json!("program"), "", json!({}));
    let line = "\nitse debug [test|attach|step|continue|eval|snapshot|stop] [args…] [--at path:line…] [--log path:line=text…] [--frame F]\n    debug a test: test [target] \
                [filter|run:N/T1], attach <host:port>, step <dbg> [over|in|out], continue|snapshot|stop <dbg>, eval <dbg> <expr>\n";
    assert!(dbg.itse(&["verbs"]).1.contains(line), "{}", dbg.itse(&["verbs"]).1);
    let env = Env::new();
    let out = env.command(&env.work(), &["verbs"]).output().unwrap();
    assert!(!stdout(&out).contains("itse debug "), "no debug adapter, no debug verb");
}

#[test]
fn a_stop_per_thread_never_answers_the_next_step() {
    let dbg = Dbg::new(json!("program"), "double-stop", json!({}));
    dbg.stopped(&["debug", "test", "--at", "src/lib.rs:2"]);
    let one = dbg.stopped(&["debug", "step", "dbg:1"]);
    assert!(one.starts_with("STOPPED debug step dbg:1 — after a step in orders::save, thread \"main\"; dbg:1\nsrc/lib.rs:3:5\n"), "the stale stop is dropped: {one}");
    let two = dbg.stopped(&["debug", "step", "dbg:1"]);
    assert!(two.starts_with("STOPPED debug step dbg:1 — after a step in orders::save, thread \"main\"; dbg:1\nsrc/lib.rs:4:5\n"), "thread 2's own stop is skipped: {two}");
    let threads: Vec<Value> = dbg.requests().into_iter().filter(|m| m["command"] == "next").map(|m| m["arguments"]["threadId"].clone()).collect();
    assert_eq!(threads, [1, 1], "every step drives the stepped thread");
}

#[test]
fn a_chatty_adapters_log_is_kept_under_4_mib() {
    let dbg = Dbg::new(json!("program"), "chatty", json!({}));
    dbg.stopped(&["debug", "test"]);
    let log = dbg.env.daemons()[0].0.join("debug/1/adapter.log");
    assert!(wait_until(Duration::from_secs(5), || fs::metadata(&log).is_ok_and(|m| m.len() <= 4 << 20)), "{:?}", fs::metadata(&log).map(|m| m.len()));
}

#[test]
fn a_stop_that_comes_after_the_wait_is_running_and_a_snapshot_waits_for_it() {
    let dbg = Dbg::new(json!("program"), "", json!({"stop_after_ms": 4000}));
    fs::create_dir_all(dbg.env.dir.join("config/itse")).unwrap();
    fs::write(dbg.env.dir.join("config/itse/config.json"), r#"{"mcp": {"debug": true}}"#).unwrap();
    let mut mcp = dbg.env.mcp(&[]);
    mcp.handshake("t");
    let started = Instant::now();
    let (error, text) = mcp.call("debug", json!({"verb": "test", "wait": 2}));
    assert!(!error && text.starts_with("RUNNING debug test — no stop yet (stop on: caught exceptions in orders); 'itse debug snapshot dbg:1' waits for it; "), "{text}");
    assert!(started.elapsed() < Duration::from_secs(4), "the call's wait bounds it: {:?}", started.elapsed());
    let status = dbg.itse(&["status"]).1;
    assert!(status.contains("\nrunning        dbg:1") && status.contains("debug test, idle ") && status.contains("\nready          dap:fixture-dap"), "{status}");
    let (_, early) = mcp.call("debug", json!({"verb": "step", "target": "dbg:1"}));
    assert_eq!(early, "ERROR debug step dbg:1 — dbg:1 has not stopped yet; 'itse debug snapshot dbg:1' waits for its stop\n");
    let (error, text) = mcp.call("debug", json!({"verb": "snapshot", "target": "dbg:1", "wait": 20}));
    assert!(
        !error && text.starts_with("STOPPED debug snapshot dbg:1 — exception in orders::save, thread \"main\"") && text.contains("\n   F1 orders::save  src/lib.rs:2\n"),
        "{text}"
    );
    assert!(dbg.itse(&["status"]).1.contains("\nstopped        dbg:1"));
}
