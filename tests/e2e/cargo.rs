//! S3's second half end to end on real cargo: `test`, `build` and `problems` over a copy of
//! tests/fixtures/rust-ws through the built-in `rust` extension's actions and matchers, the
//! `--new` baselines per caller, and that the run verbs write only their declared outputs. Without
//! cargo on PATH each test says `skipped: needs cargo`.

use super::resolver::{repo, tree};
use super::*;

const FIXTURE_WS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rust-ws");

/// A committed copy of the rust fixture in the work folder, using `rust`; None without cargo or git.
fn rust_ws(env: &Env, trust: bool) -> Option<PathBuf> {
    if !Command::new("cargo").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        eprintln!("skipped: needs cargo");
        return None;
    }
    let files = ["Cargo.toml", "src/lib.rs", "src/broken.rs.txt", "tests/orders.rs"].map(|f| (f, fs::read_to_string(Path::new(FIXTURE_WS).join(f)).unwrap()));
    if !repo(env, &files.iter().map(|(f, b)| (*f, b.as_str())).collect::<Vec<_>>()) {
        eprintln!("skipped: needs git");
        return None;
    }
    let work = env.work();
    for args in [&["use", "rust"][..], &["trust"]].into_iter().take(if trust { 2 } else { 1 }) {
        assert_eq!(itse(env, args).0, Some(0), "{args:?}");
    }
    Some(work)
}

/// `itse ARGS` in the work folder: (exit code, stdout and stderr).
fn itse(env: &Env, args: &[&str]) -> (Option<i32>, String) {
    let out = env.itse(&env.work(), args);
    (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
}

fn break_lib(work: &Path) {
    fs::copy(work.join("src/broken.rs.txt"), work.join("src/lib.rs")).unwrap();
}

#[test]
fn cargo_test_reports_each_failing_test_at_its_line_after_trust() {
    let env = Env::new();
    let Some(_) = rust_ws(&env, false) else { return };
    let (code, text) = itse(&env, &["test"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED test cargo:test — it runs project code\nuntrusted      workspace "), "{text}");
    assert!(text.contains("; next (human): itse trust"), "{text}");
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    let (is_error, text) = mcp.call("test_build_run", json!({"verb": "test"}));
    assert!(!is_error && text.starts_with("UNTRUSTED test cargo:test — ") && text.contains("next (human): itse trust in a terminal"), "{text}");
    drop(mcp);
    assert_eq!(itse(&env, &["trust"]).0, Some(0));
    let (code, text) = itse(&env, &["test"]);
    assert_eq!(code, Some(1), "{text}");
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("FAIL test work (cargo:test) — 2 failed, 2 passed, 1 skipped; ") && first.ends_with("; run:1"), "both test binaries ran: {text}");
    assert!(
        text.contains("\nT1 failed src/lib.rs:25:9 tests::saves_every_line\n     25 |         assert_eq!(saved_lines(&[1, 2, 3]), 3);\n   assertion `left == right` failed\n"),
        "{text}"
    );
    assert!(text.contains("\nT2 failed tests/orders.rs:11:5 integration_rejects_an_empty_order\n"), "{text}");
    assert!(text.contains("\ndoctests not run (I2)\n") && !text.contains("RUST_BACKTRACE"), "{text}");
    let (code, shown) = itse(&env, &["show", "run:1/T2"]);
    assert_eq!(code, Some(0), "{shown}");
    assert!(shown.contains("T2 failed tests/orders.rs:11:5 integration_rejects_an_empty_order\n") && shown.contains("   second line of the message\n"), "{shown}");
    let (code, text) = itse(&env, &["test", "rust-ws", "saves_every_line"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("FAIL test rust-ws (cargo:test) — 1 failed, 0 passed, 0 skipped; "), "the target and the filter reach cargo: {text}");
}

#[test]
fn a_compile_error_is_e1_with_the_source_line() {
    let env = Env::new();
    let Some(work) = rust_ws(&env, true) else { return };
    break_lib(&work);
    let (code, text) = itse(&env, &["build"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("FAIL build work (cargo:build) — 1 error; "), "{text}");
    assert!(text.contains("\nE1 error src/lib.rs:6:7 E0277: cannot add `&str` to `u32`: no implementation for `u32 + &str` [rustc]\n      6 |     a + \"b\"\n"), "{text}");
    let (code, text) = itse(&env, &["test"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("FAIL test work (cargo:test) — 1 error; ") && text.contains("\nE1 error src/lib.rs:6:7 "), "{text}");
}

#[test]
fn two_default_test_actions_are_an_error_with_candidates() {
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    for ext in ["rust", "gradle"] {
        assert_eq!(itse(&env, &["use", ext]).0, Some(0));
    }
    let (code, text) = itse(&env, &["test"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.starts_with("ERROR test — 2 default test actions: cargo:test, gradle:test (--action L)\n"), "{text}");
    assert!(text.contains("candidate      cargo:test           next (agent): itse test --action cargo:test\n"), "{text}");
    assert!(text.contains("candidate      gradle:test          next (agent): itse test --action gradle:test\n"), "{text}");
    let (code, text) = itse(&env, &["build", "--action", "nope:build"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.starts_with("ERROR build — no action 'nope:build' in the extensions in use; declared: "), "{text}");
}

#[test]
fn problems_new_gives_a_delta_across_two_cli_calls() {
    let env = Env::new();
    let Some(work) = rust_ws(&env, true) else { return };
    assert_eq!(itse(&env, &["problems"]), (Some(0), "EMPTY problems — 0 errors, 0 warnings\n".into()), "no action has run");
    break_lib(&work);
    assert_eq!(itse(&env, &["build"]).0, Some(1));
    let (code, text) = itse(&env, &["problems", "--new"]);
    assert_eq!(code, Some(1), "{text}");
    let first = text.lines().next().unwrap();
    assert_eq!(first, "FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; q:1", "{text}");
    assert!(text.contains("\nE1 error src/lib.rs:6:7 E0277: cannot add `&str` to `u32`: no implementation for `u32 + &str` [rustc]\n      6 |     a + \"b\"\n"), "{text}");
    assert!(text.contains("\nfrom cargo:build run:1, ended "), "{text}");
    let (code, text) = itse(&env, &["problems", "--new"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "), "{text}");
    let (code, text) = itse(&env, &["problems", "tests/"]);
    assert_eq!((code, text.lines().next()), (Some(0), Some("EMPTY problems tests/ — 0 errors, 0 warnings")), "{text}");
    fs::copy(Path::new(FIXTURE_WS).join("src/lib.rs"), work.join("src/lib.rs")).unwrap();
    assert_eq!(itse(&env, &["build"]).0, Some(0));
    let (code, text) = itse(&env, &["problems", "--new"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 0 unchanged, 1 fixed since "), "{text}");
}

#[test]
fn a_run_that_outlasts_the_wait_is_parsed_when_it_ends() {
    let env = Env::new();
    let Some(work) = rust_ws(&env, true) else { return };
    break_lib(&work);
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    let (is_error, text) = mcp.call("test_build_run", json!({"verb": "build", "wait": 0}));
    assert!(!is_error && text.starts_with("RUNNING build work (cargo:build) — "), "{text}");
    let end = Instant::now() + Duration::from_secs(120);
    let shown = loop {
        let (_, shown) = itse(&env, &["show", "run:1"]);
        if !shown.starts_with("RUNNING") || Instant::now() > end {
            break shown;
        }
        thread::sleep(Duration::from_millis(200));
    };
    assert!(shown.starts_with("FAIL run cargo:build — 1 error; ") && shown.contains("\nE1 error src/lib.rs:6:7 E0277: "), "parsed by the daemon, not the verb: {shown}");
    let (code, text) = itse(&env, &["problems"]);
    assert!(code == Some(1) && text.starts_with("FAIL problems — 1 error, 0 warnings; q:1\n"), "{text}");
}

/// `problems --new` over a hook-mode connection with this session id, as `itse hook` will call it.
fn hook_problems(env: &Env, session: &str) -> String {
    let conn = UnixStream::connect(env.socket()).unwrap();
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut ask = |method: &str, params: Value| {
        writeln!(&conn, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
    };
    ask("hello", json!({"proto": 1, "mode": "hook", "env": {"ITSE_SESSION": session}}));
    ask("call", json!({"verb": "problems", "args": ["--new"]}))["text"].as_str().unwrap().to_string()
}

#[test]
fn two_hook_sessions_keep_separate_baselines() {
    let env = Env::new();
    let Some(work) = rust_ws(&env, true) else { return };
    break_lib(&work);
    assert_eq!(itse(&env, &["build"]).0, Some(1));
    let first = |text: String| text.lines().next().unwrap_or_default().to_string();
    assert_eq!(first(hook_problems(&env, "session-a")), "FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; q:1");
    assert!(first(hook_problems(&env, "session-a")).starts_with("EMPTY problems — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "));
    assert_eq!(first(hook_problems(&env, "session-b")), "FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; q:2", "another session has its own baseline");
    let (_, text) = itse(&env, &["problems", "--new"]);
    assert!(text.starts_with("FAIL problems — 1 new error"), "and so has the plain CLI (cli:cli): {text}");
    let baselines: Value = serde_json::from_slice(&fs::read(env.daemons()[0].0.join("baselines.json")).unwrap()).unwrap();
    let mut keys: Vec<&str> = baselines.as_object().unwrap().keys().map(|k| k.split_once(':').unwrap().0).collect();
    keys.sort();
    assert_eq!(keys, ["cli", "hook", "hook"]);
    assert!(!baselines.to_string().contains("session-a"), "session ids are hashed: {baselines}");
}

#[test]
fn run_verbs_change_only_declared_outputs() {
    let env = Env::new();
    let Some(work) = rust_ws(&env, true) else { return };
    let before = tree(&work);
    assert!(before.contains_key(&work.join(".git/index")));
    assert_eq!(itse(&env, &["build"]).0, Some(0));
    assert_eq!(itse(&env, &["test"]).0, Some(1));
    let after = tree(&work);
    let changed: Vec<&PathBuf> = after.iter().filter(|(p, m)| before.get(*p) != Some(m)).map(|(p, _)| p).chain(before.keys().filter(|p| !after.contains_key(*p))).collect();
    let outside: Vec<_> = changed.iter().filter(|p| !p.starts_with(work.join("target")) && **p != &work.join("Cargo.lock") && **p != &work).collect();
    assert!(outside.is_empty(), "written outside target/ and Cargo.lock: {outside:?}");
    assert!(changed.iter().any(|p| p.starts_with(work.join("target"))) && after.contains_key(&work.join("Cargo.lock")));
}
