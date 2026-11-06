use std::io::{Read, Write};
use std::net::TcpListener;

use super::*;

#[test]
fn a_loopback_adapter_port_is_connected() {
    // The test listens, standing in for an adapter; the core never does.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut buf = [0u8; 4];
        listener.accept().unwrap().0.read_exact(&mut buf).unwrap();
        buf
    });
    connect_loopback("127.0.0.1", port, 1).unwrap().write_all(b"ping").unwrap();
    assert_eq!(&server.join().unwrap(), b"ping");
    if let Ok(v6) = TcpListener::bind("[::1]:0") {
        let port = v6.local_addr().unwrap().port();
        assert!(connect_loopback("[::1]", port, 1).is_ok());
        assert!(connect_loopback("::1", port, 1).is_ok());
    }
}

#[test]
fn a_non_loopback_address_is_refused() {
    for host in ["10.0.0.1", "127.0.0.2", "0.0.0.0", "::", "::ffff:127.0.0.1", "localhost", "example.com", "", "192.168.1.1"] {
        let e = connect_loopback(host, 5005, 50).unwrap_err().to_string();
        assert_eq!(e, format!("{host} is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only"));
    }
}

#[test]
fn a_closed_loopback_port_is_an_error_that_names_it_after_its_tries() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let started = Instant::now();
    let e = connect_loopback("127.0.0.1", port, 3).unwrap_err().to_string();
    assert_eq!(e, format!("connecting to 127.0.0.1 port {port}"));
    assert!(started.elapsed() >= Duration::from_millis(200), "three tries, 100 ms apart: {:?}", started.elapsed());
}

#[test]
fn a_debug_server_port_is_read_in_every_shape_lsp_commands_answer_with() {
    let local = |p: u16| ("127.0.0.1".to_string(), p);
    assert_eq!(address(&json!(5005)).unwrap(), local(5005));
    assert_eq!(address(&json!("5005")).unwrap(), local(5005));
    assert_eq!(address(&json!({"port": 5005})).unwrap(), local(5005));
    assert_eq!(address(&json!({"port": "5005", "host": "::1"})).unwrap(), ("::1".to_string(), 5005));
    assert_eq!(address(&json!("127.0.0.1:5005")).unwrap(), local(5005));
    assert_eq!(address(&json!("[::1]:5005")).unwrap(), ("[::1]".to_string(), 5005));
    assert_eq!(address(&json!("10.0.0.1:5005")).unwrap(), ("10.0.0.1".to_string(), 5005), "read here, refused by connect_loopback");
    assert_eq!(address(&json!({"ok": true})).unwrap_err().to_string(), r#"{"ok":true} is not a port or host:port"#);
    assert!(address(&json!("")).is_err() && address(&json!(70000)).is_err());
}

#[test]
fn project_frames_are_the_roots_files_outside_build_and_target() {
    let tmp = sys::tests::TempDir::new("itse-dap");
    let root = tmp.0.as_path();
    for f in ["src/lib.rs", "target/debug/build.rs", "data/build/gen/G.kt"] {
        fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
        fs::write(root.join(f), "x\n").unwrap();
    }
    let at = |p: &str| project(root, &json!({"source": {"path": p}}));
    assert_eq!(at(&root.join("src/lib.rs").display().to_string()).as_deref(), Some("src/lib.rs"));
    assert_eq!(at(&format!("file://{}/src/lib.rs", root.display())).as_deref(), Some("src/lib.rs"));
    assert_eq!(at("src/lib.rs").as_deref(), Some("src/lib.rs"), "a path relative to the root");
    let name = root.file_name().unwrap().to_str().unwrap();
    assert_eq!(at(&format!("/mnt/ci/{name}/src/lib.rs")).as_deref(), Some("src/lib.rs"), "another checkout of the same repo");
    let spaced = root.join("my ws");
    fs::create_dir_all(spaced.join("src")).unwrap();
    fs::write(spaced.join("src/lib.rs"), "x\n").unwrap();
    assert_eq!(project(&spaced, &json!({"source": {"path": format!("file://{}/my%20ws/src/lib.rs", root.display())}})).as_deref(), Some("src/lib.rs"), "a URI, decoded");
    let outside = root.parent().unwrap().join("x.rs");
    fs::write(&outside, "x\n").unwrap();
    for not in [
        root.join("target/debug/build.rs").display().to_string(),
        root.join("data/build/gen/G.kt").display().to_string(),
        "/rustc/abc/library/core/src/panicking.rs".into(),
        format!("{}/../x.rs", root.display()),
        "../x.rs".into(),
    ] {
        assert_eq!(at(&not), None, "{not}");
    }
    assert_eq!(project(root, &json!({"name": "no source"})), None);
    let _ = fs::remove_file(outside);
}

fn caps(filters: Value, more: Value) -> Value {
    let mut caps = json!({"exceptionBreakpointFilters": filters});
    caps.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
    caps
}

#[test]
fn caught_exceptions_are_scoped_only_by_a_declared_condition_the_adapter_takes() {
    let rule = Rule { caught: Some(vec!["com.acme".into(), "org.x".into()]), ..Rule::default() };
    let both = json!([{"filter": "caught", "supportsCondition": true}, {"filter": "uncaught"}]);
    let takes = caps(both.clone(), json!({"supportsExceptionFilterOptions": true, "supportsExceptionOptions": true}));
    let scoped = rule.exceptions(&takes, Some("in(${packages})"));
    assert_eq!(scoped, (json!({"filters": ["uncaught"], "filterOptions": [{"filterId": "caught", "condition": "in(com.acme,org.x)"}]}), false));
    let plain = (json!({"filters": ["caught", "uncaught"]}), true);
    assert_eq!(rule.exceptions(&takes, None), plain, "no condition declared: the plain filter, never exceptionOptions, whose paths name exception types");
    assert_eq!(rule.exceptions(&caps(both.clone(), json!({})), Some("in(${packages})")), plain, "an adapter that takes no condition");
    let unscoped = Rule { caught: Some(vec![]), ..Rule::default() };
    assert_eq!(unscoped.exceptions(&takes, Some("in(${packages})")), plain, "no packages to scope by");
    assert_eq!(rule.exceptions(&caps(json!([{"filter": "caught"}]), json!({})), None), (json!({"filters": ["caught"]}), true));
    assert_eq!(rule.exceptions(&caps(json!([{"filter": "uncaught"}]), json!({})), None), (json!({"filters": ["uncaught"]}), false), "no caught filter to skip on");
    assert_eq!(Rule::default().exceptions(&takes, None), (json!({"filters": []}), false), "line breakpoints set no exception filter");
}

#[test]
fn at_and_log_flags_become_breakpoints_and_logpoints_by_source() {
    let tmp = sys::tests::TempDir::new("itse-dap");
    fs::create_dir_all(tmp.0.join("src")).unwrap();
    for f in ["src/a.rs", "src/b.rs"] {
        fs::write(tmp.0.join(f), "x\n").unwrap();
    }
    let args = Args { flags: [("at".into(), json!(["src/a.rs:3", "src/b.rs:9"])), ("log".into(), json!(["src/a.rs:5=order {order}"]))].into(), ..Args::default() };
    let r = rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &args, None, &[]).unwrap();
    assert_eq!((r.text.as_str(), r.logs.clone()), ("breakpoints at src/a.rs:3, src/b.rs:9", vec!["src/a.rs:5".to_string()]));
    assert_eq!(r.lines[&tmp.0.join("src/a.rs")], [json!({"line": 3}), json!({"line": 5, "logMessage": "order {order}"})]);
    assert_eq!((r.lines.len(), r.caught.is_none()), (2, true));
    let logs_only = Args { flags: [("log".into(), json!(["src/a.rs:5=x"]))].into(), ..Args::default() };
    let r = rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &logs_only, None, &["orders".into()]).unwrap();
    assert_eq!((r.text.as_str(), r.caught), ("caught exceptions in orders", Some(vec!["orders".to_string()])), "a logpoint never stops, so it is no stop rule");
    let bad = Args { flags: [("at".into(), json!(["src/a.rs"]))].into(), ..Args::default() };
    assert_eq!(rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &bad, None, &[]).unwrap_err().to_string(), "'src/a.rs' is not path:line");
    let from_src = Args { flags: [("at".into(), json!(["a.rs:3"]))].into(), ..Args::default() };
    let r = rule(&tmp.0, &tmp.0.join("src"), &Store(tmp.0.clone()), &from_src, None, &[]).unwrap();
    assert_eq!((r.text.as_str(), r.lines.keys().collect::<Vec<_>>()), ("breakpoints at src/a.rs:3", vec![&tmp.0.join("src/a.rs")]), "from the caller's folder");
    let missing = Args { flags: [("at".into(), json!(["a.rs:3"]))].into(), ..Args::default() };
    assert_eq!(rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &missing, None, &[]).unwrap_err().to_string(), "a.rs: no such file", "refused before anything starts");
}

#[test]
fn a_logpoint_without_its_text_is_refused_since_it_would_stop() {
    let tmp = sys::tests::TempDir::new("itse-dap");
    fs::create_dir_all(tmp.0.join("src")).unwrap();
    fs::write(tmp.0.join("src/a.rs"), "x\n").unwrap();
    for log in ["src/a.rs:5", "src/a.rs:5="] {
        let args = Args { flags: [("log".into(), json!([log]))].into(), ..Args::default() };
        assert_eq!(rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &args, None, &[]).unwrap_err().to_string(), format!("'{log}' is not path:line=text"));
    }
}

#[test]
fn only_the_events_a_session_waits_on_are_queued_and_telemetry_is_no_output() {
    let events = Events::default();
    for (event, body) in [("module", json!({})), ("loadedSource", json!({})), ("output", json!({"category": "telemetry", "output": "{\"blob\": 1}"}))] {
        events.notify(event, body);
    }
    events.notify("output", json!({"category": "stdout", "output": "hello\n"}));
    events.notify("stopped", json!({"reason": "step"}));
    let q = lock(&events.q);
    assert_eq!((q.0.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), q.1.iter().cloned().collect::<Vec<_>>()), (vec!["stopped"], vec!["hello".to_string()]));
}

fn var(name: &str, value: &str, children: Vec<Value>) -> Value {
    json!({"name": name, "type": "T", "value": value, "children": children})
}

fn snap(vars: Vec<Value>, frames: usize) -> Value {
    let frame = |i: usize| {
        json!({"id": i, "name": format!("f{i}"), "path": "src/lib.rs", "line": 2, "col": 5, "src": [[1, "// orders"], [2, "pub fn save() {}"]],
        "scopes": [{"name": "Locals", "vars": vars}]})
    };
    json!({"reason": "exception", "thread": {"id": 1, "name": "main"}, "frames": (0..frames).map(frame).collect::<Vec<_>>(), "framework": 7, "exception": null, "output": ["a", "b"]})
}

#[test]
fn the_snapshot_text_shows_the_frame_its_locals_and_the_frames() {
    let (t, cut) = text(&snap(vec![var("order", "Order(id=O-17)", vec![var("id", "\"O-17\"", vec![])]), var("n", "3", vec![])], 2), 0);
    let want = "src/lib.rs:2:5\n      1 | // orders\n  >   2 | pub fn save() {}\nlocals\n   order  T  Order(id=O-17)\n      id  T  \"O-17\"\n   n      T  3\n\
                frames (project)\n   F1 f0  src/lib.rs:2\n   F2 f1  src/lib.rs:2\n   … 7 framework frames\noutput: a\noutput: b";
    assert_eq!((t.as_str(), cut), (want, false));
}

#[test]
fn past_4000_chars_children_go_then_later_frames_then_the_rest_is_cut() {
    let long = "x".repeat(150);
    let kids = || (0..10).map(|i| var(&format!("c{i}"), &long, vec![])).collect::<Vec<_>>();
    let (t, cut) = text(&snap((0..12).map(|i| var(&format!("v{i}"), &long, kids())).collect(), 3), 0);
    assert!(cut && !t.contains("\n      c0") && t.contains("\n   F3 f2 "), "children dropped first: {t}");
    let (t, cut) = text(&snap((0..20).map(|i| var(&format!("v{i}"), &long, vec![])).collect(), 30), 0);
    assert!(cut && t.contains("\n   F1 f0 ") && !t.contains("\n   F2 ") && t.contains("   … 29 more project frames"), "then the later frames: {t}");
    let mut big = snap((0..40).map(|i| var(&format!("v{i}"), &long, vec![])).collect(), 1);
    big["exception"] = json!({"exceptionId": "AssertionError", "description": "expected 2, got 3"});
    let (t, cut) = text(&big, 0);
    let tail = "\n   … 17 more locals\nexception AssertionError: expected 2, got 3\nframes (project)\n   F1 f0  src/lib.rs:2\n   … 7 framework frames\noutput: a\noutput: b";
    assert!(cut && t.chars().count() <= CAP && t.ends_with(tail) && t.contains("\n   v22 "), "then fewer locals, the exception and frames kept: {t}");
}

#[test]
fn a_huge_variable_name_neither_panics_nor_widens_the_columns() {
    let (t, cut) = text(&snap(vec![var(&"n".repeat(70_000), "1", vec![])], 1), 0);
    assert!(cut && t.contains("\nlocals\n   … 1 more locals\nframes (project)\n"), "a width past u16::MAX would panic: {t}");
    let (t, _) = text(&snap(vec![var(&"n".repeat(100), "1", vec![]), var("short", "2", vec![])], 1), 0);
    assert!(t.contains(&format!("\n   short{}T  2\n", " ".repeat(37))), "{t}");
}

#[test]
fn a_snapshot_cut_short_is_marked_cut_and_frames_past_the_last_are_refused() {
    let mut s = snap(vec![var("n", "3", vec![])], 2);
    assert!(!text(&s, 0).1);
    s["cut"] = json!(true);
    assert!(text(&s, 0).1, "a request failed or the budget ran out");
    assert_eq!(frame_at(&s, 1).unwrap()["name"], "f1");
    assert_eq!(frame_at(&s, 2).unwrap_err().to_string(), "the stop has 2 project frames, so no F3");
    assert!(frame_at(&snap(vec![], 0), 0).unwrap().is_null(), "F1 at a stop outside the project");
}

#[test]
fn without_project_packages_the_newest_test_results_names_give_the_prefixes() {
    let tmp = sys::tests::TempDir::new("itse-dap");
    let item = |msg: &str| Item { id: "T1".into(), kind: "failed".into(), msg: msg.into(), ..Item::default() };
    let names = [item("orders::tests::saves"), item("com.acme.data.OrderTest.saves order()"), item("orders::tests::loads"), item("tests::alone"), item("Plain")];
    assert_eq!(prefixes(&names), ["com.acme.data", "orders"]);
    let o = Outcome { id: Some("run:3".into()), items: vec![item("billing::tests::bills")], ..Outcome::new("test", Verdict::Fail, "ws (fix:test)", "1 failed") };
    fs::create_dir_all(tmp.0.join("runs/3")).unwrap();
    Store(tmp.0.clone()).save(&o, "s").unwrap();
    let r = rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &Args::default(), Some("ws ("), &[]).unwrap();
    assert_eq!((r.text.as_str(), r.caught), ("caught exceptions in billing", Some(vec!["billing".to_string()])), "a failure without a frame: its name");
    let r = rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &Args::default(), Some("ws ("), &["orders".into()]).unwrap();
    assert_eq!(r.caught, Some(vec!["orders".to_string()]), "declared packages come first");
    assert_eq!(rule(&tmp.0, &tmp.0, &Store(tmp.0.clone()), &Args::default(), Some("other ("), &[]).unwrap().text, "caught exceptions in project code");
}
