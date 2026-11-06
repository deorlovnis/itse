//! The LSP client end to end against `itse-fixture lsp`: servers started on demand and stopped
//! when idle, the cold index flag, crashes and restarts, `status` during a start, trust, and the
//! sync from disk, which writes nothing into the workspace.

use super::resolver::{git, repo, tree};
use super::*;

pub(super) const LIB: &str = "// orders\npub fn save() {}\n";

pub(super) fn symbols() -> Value {
    json!([{"name": "save", "kind": 12, "container": "orders", "path": "src/lib.rs", "line": 1, "character": 7},
           {"name": "saveAll", "kind": 12, "container": "orders", "path": "src/lib.rs", "line": 1, "character": 7}])
}

/// A committed workspace that uses the extension `fix`, whose one server is the fixture LSP with
/// this script and mode, its messages logged; `code` marks it as running project code.
pub(super) struct Fix {
    pub(super) env: Env,
    pub(super) log: PathBuf,
    /// Env for every call, so the daemon the first call starts has it.
    vars: Vec<(&'static str, &'static str)>,
}

/// Servers stay for a minute unless a test wants the idle stop.
const IDLE: &[(&str, &str)] = &[("ITSE_SERVER_IDLE_S", "60")];

impl Fix {
    pub(super) fn new(script: Value, mode: &str, code: bool) -> Option<Self> {
        Self::with(script, mode, code, IDLE)
    }

    fn with(script: Value, mode: &str, code: bool, vars: &[(&'static str, &'static str)]) -> Option<Self> {
        let env = Env::new();
        if !repo(&env, &[("src/lib.rs", LIB), ("notes.md", "x\n")]) {
            eprintln!("skipped: needs git");
            return None;
        }
        let log = env.dir.join("fixture.log");
        let server = json!({"id": "fixture", "protocol": "lsp", "command": [FIXTURE, "lsp"], "filetypes": ["rs"], "languages": {"rs": "rust"}, "settings": {"fixture": {"level": 2}},
            "env": {"ITSE_FIXTURE_SCRIPT": script.to_string(), "ITSE_FIXTURE_MODE": mode, "ITSE_FIXTURE_LOG": log}, "executes_project_code": code});
        let ext = env.dir.join("fix.json");
        fs::write(&ext, json!({"itse_ext": 1, "id": "fix", "servers": [server]}).to_string()).unwrap();
        let fix = Self { env, log, vars: vars.to_vec() };
        for args in [&["ext", "add", ext.to_str().unwrap()][..], &["use", "fix"]] {
            let (code, text) = fix.itse(args);
            assert_eq!(code, Some(0), "{args:?}: {text}");
        }
        Some(fix)
    }

    /// `itse ARGS` in the work folder: (exit code, stdout and stderr).
    pub(super) fn itse(&self, args: &[&str]) -> (Option<i32>, String) {
        self.itse_env(args, &[])
    }

    pub(super) fn itse_env(&self, args: &[&str], extra: &[(&str, &str)]) -> (Option<i32>, String) {
        let out = self.env.command(&self.env.work(), args).envs(self.vars.iter().chain(extra).copied()).output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    }

    /// The messages the fixture got, from every start.
    fn messages(&self) -> Vec<Value> {
        fs::read_to_string(&self.log).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn methods(&self) -> Vec<String> {
        self.messages().iter().filter_map(|m| m["method"].as_str().map(String::from)).collect()
    }

    /// The server's pid in procs.json, while it is recorded there.
    fn server_pid(&self) -> Option<i32> {
        let procs: Value = serde_json::from_slice(&fs::read(self.env.daemons().first()?.0.join("procs.json")).ok()?).ok()?;
        procs.as_array()?.iter().find(|p| p["handle"] == "lsp:fixture").and_then(|p| p["pid"].as_i64()).map(|p| p as i32)
    }

    /// The `lsp:fixture` row of `status`.
    fn row(&self) -> String {
        let (code, text) = self.itse(&["status"]);
        assert_eq!(code, Some(0), "{text}");
        text.lines().find(|l| l.split_whitespace().any(|w| w == "lsp:fixture")).unwrap_or_else(|| panic!("no lsp:fixture row: {text}")).to_string()
    }
}

#[test]
fn the_first_query_on_a_cold_index_says_so() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "index_ms": 3500}), "", false) else { return };
    let started = Instant::now();
    let (code, text) = fix.itse(&["find", "orders::save"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK find orders::save — 1 match; fixture index cold (Indexing 0/3 (0%)); q:1\n"), "{text}");
    assert!(text.contains("\nS1 function src/lib.rs:2:8 orders::save\n      2 | pub fn save() {}\n"), "{text}");
    assert!(started.elapsed() >= Duration::from_secs(3), "a cold server gets 3 s to settle first: {:?}", started.elapsed());
    assert!(text.contains("\nfixture not settled after 3 s: the answer may miss the latest edits\n"), "{text}");
    let messages = fix.messages();
    let init = &messages[0]["params"];
    assert_eq!((init["clientInfo"]["name"].as_str(), init["rootUri"].as_str()), (Some("itse"), Some(format!("file://{}", fix.env.work().display()).as_str())));
    let config = messages.iter().find(|m| m["id"] == "cfg-1").expect("the configuration request was answered");
    assert_eq!(config["result"], json!([{"level": 2}]));
    assert!(messages.iter().any(|m| m["id"] == "prog-1" && m["result"].is_null()), "the progress token was accepted");
    assert!(fix.row().contains("; index cold (Indexing 0/3 (0%)); "), "status reads the flag, it doesn't settle: {}", fix.row());
    thread::sleep(Duration::from_millis(3600).saturating_sub(started.elapsed()));
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK find save — 1 match; fixture index warm; q:2\n"), "{text}");
    let (code, text) = fix.itse(&["find", "sav"]);
    assert!(code == Some(0) && text.starts_with("OK find sav — 2 matches; "), "a prefix finds both: {text}");
    let (code, text) = fix.itse(&["find", "nothing"]);
    assert_eq!((code, text.as_str()), (Some(1), "EMPTY find nothing — 0 matches; fixture index warm\n"), "settled, and nothing cut off");
    let row = fix.row();
    assert!(row.starts_with("up             lsp:fixture          pid ") && row.contains("; index warm; 0 diagnostics; "), "{row}");
    assert!(row.ends_with(" window/logMessage: fixture indexing"), "the last message: {row}");
    // The usage log says which answers were warm, and so does stats.
    let warm: Vec<Value> = fix.env.usage().iter().filter(|r| r["verb"] == "find").map(|r| r["warm"].clone()).collect();
    assert_eq!(warm, [false, true, true, true]);
    let stats = stdout(&fix.env.itse(&fix.env.work(), &["stats", "--by", "verb"]));
    assert!(stats.lines().any(|l| l.starts_with("find ") && l.ends_with(" 75%")), "{stats}");
}

#[test]
fn a_long_fuzzy_answer_without_the_name_says_it_may_be_cut_off() {
    let many: Vec<Value> = (0..120).map(|n| json!({"name": format!("as_save_{n}"), "kind": 12, "path": "src/lib.rs", "line": 1, "character": 7})).collect();
    let Some(fix) = Fix::new(json!({"symbols": many}), "", false) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("\nfixture sent 120 fuzzy matches and none named save; its list may be cut off\n"), "{text}");
}

#[test]
fn a_crashed_server_restarts_on_next_use() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let (code, text) = fix.itse(&["find", "FIXTURE_CRASH"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.starts_with("ERROR find — server_crash: lsp:fixture ended during the call (eof); the next call starts it again"), "{text}");
    assert!(wait_until(Duration::from_secs(2), || fix.row().starts_with("crashed")), "{}", fix.row());
    let row = fix.row();
    assert!(row.starts_with("crashed        lsp:fixture          exit 3; log: ") && row.ends_with("; the next use starts it again"), "{row}");
    assert_eq!(fix.server_pid(), None, "a crashed server leaves procs.json");
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("\nfixture restarted after a crash (exit 3)\n"), "{text}");
    assert_eq!(fix.methods().iter().filter(|m| *m == "initialize").count(), 2);
}

#[test]
fn a_server_that_dies_before_initialize_is_start_failed_once_per_call() {
    let Some(fix) = Fix::new(json!({}), "exit:2", false) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.starts_with("ERROR find — lsp:fixture: start_failed: exit 2 before initialize answered (initialize: "), "{text}");
    assert!(fix.row().starts_with("crashed        lsp:fixture          exit 2 before initialize answered"), "{}", fix.row());
    let (_, text) = fix.itse(&["find", "save"]);
    assert!(text.contains("start_failed: exit 2"), "the next call tries again: {text}");
}

#[test]
fn an_idle_server_is_stopped_with_shutdown_and_exit() {
    let Some(fix) = Fix::with(json!({"symbols": symbols()}), "", false, &[("ITSE_SERVER_IDLE_S", "2"), ("ITSE_DAEMON_IDLE_S", "30")]) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(0), "{text}");
    let pid = fix.server_pid().expect("the server is in procs.json");
    assert!(alive(pid));
    let daemon = fix.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    assert!(wait_until(Duration::from_secs(6), || !alive(pid) && fix.server_pid().is_none()), "ITSE_SERVER_IDLE_S=2");
    assert_eq!(fix.row(), "stopped        lsp:fixture          starts on first use");
    assert!(alive(daemon), "the daemon stays; it only stopped the server");
    let methods = fix.methods();
    assert_eq!(methods[methods.len() - 2..], ["shutdown", "exit"], "{methods:?}");
}

#[test]
fn status_answers_while_a_server_is_starting() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "slow_ms": 3000}), "slow", false) else { return };
    assert!(fix.row().starts_with("stopped        lsp:fixture"));
    let find = fix.env.command(&fix.env.work(), &["find", "save"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut rows = Vec::new();
    while rows.len() < 30 && !rows.last().is_some_and(|r: &String| r.starts_with("up")) {
        let started = Instant::now();
        rows.push(fix.row());
        assert!(started.elapsed() < Duration::from_secs(1), "status took {:?}", started.elapsed());
        thread::sleep(Duration::from_millis(200));
    }
    assert!(rows.iter().any(|r| r.starts_with("starting (") && r.contains(" s) lsp:fixture")), "{rows:?}");
    let out = find.wait_with_output().unwrap();
    assert!(out.status.success() && stdout(&out).starts_with("OK find save — 1 match; "), "{}", stdout(&out));
}

#[test]
fn untrusted_workspace_starts_no_project_code_server() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", true) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(
        text.starts_with(
            "UNTRUSTED find save — fixture runs project code\nuntrusted      workspace            project code runs in fixture; next (human): itse trust in a terminal\n"
        ),
        "{text}"
    );
    let (code, text) = fix.itse(&["problems"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED problems — fixture runs project code; 0 errors, 0 warnings\nuntrusted      workspace "), "unknown, not none: {text}");
    let (code, text) = fix.itse(&["problems", "notes.md"]);
    assert_eq!((code, text.as_str()), (Some(0), "EMPTY problems notes.md — 0 errors, 0 warnings\n"), "a file it doesn't take");
    let (_, status) = fix.itse(&["status"]);
    assert!(!fix.log.exists() && fix.server_pid().is_none() && !status.contains("lsp:fixture"), "nothing started: {status}");
    assert_eq!(fix.itse(&["trust"]).0, Some(0));
    let (code, text) = fix.itse(&["find", "save"]);
    assert!(code == Some(0) && text.starts_with("OK find save — 1 match"), "{text}");
    let pid = fix.server_pid().unwrap();
    assert_eq!(fix.itse(&["trust", "--revoke"]).0, Some(0));
    assert!(!alive(pid) && fix.server_pid().is_none(), "a revoked trust stops the server that runs project code");
    assert_eq!(fix.itse(&["find", "save"]).0, Some(3));
}

#[test]
fn nothing_writes_inside_the_workspace_with_a_server_running() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    let index = || fs::metadata(work.join(".git/index")).map(|m| (m.ino(), m.mtime(), m.mtime_nsec())).unwrap();
    let (before, index_before) = (tree(&work), index());
    let (code, text) = fix.itse(&["outline", "src/lib.rs"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK outline src/lib.rs — 2 symbols; fixture index ") && text.contains("\nS1 function src/lib.rs:2:8 save\n"), "{text}");
    for args in [&["find", "save"][..], &["status"]] {
        assert_eq!(fix.itse(args).0, Some(0), "{args:?}");
    }
    // Edits from outside: the opened file changes, a new one appears; the next query tells the server.
    fs::write(work.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    fs::write(work.join("src/new.rs"), "fn n() {}\n").unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let uri = |p: &str| format!("file://{}/{p}", work.display());
    let told: Vec<(String, String)> = fix
        .messages()
        .iter()
        .filter(|m| m["method"].as_str().is_some_and(|m| m.starts_with("textDocument/did")))
        .map(|m| (m["method"].as_str().unwrap().into(), m["params"]["textDocument"]["uri"].as_str().unwrap().into()))
        .collect();
    let want = |method: &str, p: &str| (method.to_string(), uri(p));
    assert_eq!(
        told,
        [
            want("textDocument/didOpen", "src/lib.rs"),
            want("textDocument/didChange", "src/lib.rs"),
            want("textDocument/didSave", "src/lib.rs"),
            want("textDocument/didOpen", "src/new.rs"),
            want("textDocument/didSave", "src/new.rs")
        ]
    );
    assert!(fix.row().contains("; 1 diagnostic; "), "the fixture's push for the edit was kept: {}", fix.row());
    fs::remove_file(work.join("src/lib.rs")).unwrap();
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    assert!(fix.messages().iter().any(|m| m["method"] == "textDocument/didClose" && m["params"]["textDocument"]["uri"] == uri("src/lib.rs")), "a deleted document is closed");
    let (code, text) = fix.itse(&["stop", "lsp:fixture"]);
    assert_eq!((code, text.as_str()), (Some(0), "STOPPED stop lsp:fixture — stopped\n"));
    assert_eq!(index(), index_before, ".git/index: same inode and mtime");
    let after = tree(&work);
    let mut changed: Vec<_> =
        after.iter().filter(|(p, m)| before.get(*p) != Some(m)).map(|(p, _)| p.clone()).chain(before.keys().filter(|p| !after.contains_key(*p)).cloned()).collect();
    changed.sort();
    assert_eq!(changed, [work.join("src"), work.join("src/lib.rs"), work.join("src/new.rs")], "only the test's own edits");
}

#[test]
fn a_killed_daemon_leaves_the_server_gone_and_the_next_use_cold() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "index_ms": 3500}), "", false) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let (daemon, server) = (fix.env.daemon_json()["pid"].as_i64().unwrap() as i32, fix.server_pid().unwrap());
    signal(daemon, "KILL");
    assert!(wait_until(Duration::from_secs(3), || !alive(daemon) && !alive(server)), "the server ends with its pipes");
    assert_eq!(fix.row(), "gone           lsp:fixture          daemon restarted; the next use starts it cold");
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains(" index cold") && text.contains("\nfixture started cold (daemon restarted)\n"), "{text}");
}

// --- navigation by name and LSP problems ---

const REGION: &str = "// regions\npub struct Region;\n\nimpl Region {\n    pub fn save(&self) {}\n}\n";
const ORDERS: &str = "pub fn save() {}\npub fn run() {\n    save();\n}\n";

/// Two `save`s, a method of `Region` in src/data/region.rs and a free function in src/orders.rs
/// that the server gives no container, and the `Region` struct; two references and a hover.
fn nav() -> Option<Fix> {
    let symbols = json!([{"name": "save", "kind": 6, "container": "Region", "path": "src/data/region.rs", "line": 4, "character": 11},
        {"name": "save", "kind": 12, "path": "src/orders.rs", "line": 0, "character": 7},
        {"name": "Region", "kind": 23, "path": "src/data/region.rs", "line": 1, "character": 11}]);
    let refs = json!([{"path": "src/orders.rs", "line": 2, "character": 4}, {"path": "src/lib.rs", "line": 1, "character": 7}]);
    let fix = Fix::new(json!({"symbols": symbols, "references": refs, "hover": "```rust\npub fn save(&self)\n```\n\n---\n\nSaves the region."}), "", false)?;
    for (path, body) in [("src/data/region.rs", REGION), ("src/orders.rs", ORDERS)] {
        fs::create_dir_all(fix.env.work().join(path).parent().unwrap()).unwrap();
        fs::write(fix.env.work().join(path), body).unwrap();
    }
    Some(fix)
}

#[test]
fn find_by_name_returns_candidates_on_ambiguity() {
    let Some(fix) = nav() else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK find save — 2 matches; fixture index "), "{text}");
    assert!(text.contains("\nS1 method src/data/region.rs:5:12 Region::save\n      5 |     pub fn save(&self) {}\nS2 function src/orders.rs:1:8 save\n"), "{text}");
    // Item ids, spelled out so they don't read as names elsewhere.
    let [s1, s2] = [1, 2].map(|n| format!("S{n}"));
    let (code, text) = fix.itse(&["refs", "save"]);
    assert_eq!(code, Some(0), "several matches are an answer, not nothing: {text}");
    let head = format!(
        "OK refs save — 2 matches; fixture index warm; q:2\nambiguous      save                 2 matches: pick one by its id; next (agent): itse refs q:2/{s1}\nS1 method "
    );
    assert!(text.starts_with(&head) && text.contains("\nS2 function src/orders.rs:1:8 save\n"), "{text}");
    let mut mcp = fix.env.mcp(&fix.vars);
    mcp.handshake("e2e");
    let (_, over_mcp) = mcp.call("problems_symbols", json!({"verb": "info", "target": "save"}));
    assert!(over_mcp.contains(&format!("; next (agent): problems_symbols {{verb: \"info\", args: [\"q:3/{s1}\"]}}\n")), "{over_mcp}");
    drop(mcp);
    let (code, text) = fix.itse(&["refs", &s2]);
    assert_eq!(code, Some(0), "{text}");
    let want = format!(
        "OK refs {s2} — 2 references in 2 files; fixture index warm; q:4\nS1 function src/orders.rs:1:8 save\n      1 | pub fn save() {{}}\n\
        R1 ref src/lib.rs:2:8\n      2 | pub fn save() {{}}\nR2 ref src/orders.rs:3:5\n      3 |     save();\n\
        {s2} from q:2, the newest result holding it\nprecise references only; strings and comments not searched\n"
    );
    assert_eq!(text, want, "the candidate picked from this session's newest result, not the other session's, references by file");
    // A qualified id names its result, whatever was asked meanwhile.
    assert_eq!(fix.itse(&["outline", "src/data/region.rs"]).0, Some(0));
    let (code, text) = fix.itse(&["refs", &format!("q:2/{s2}")]);
    assert!(code == Some(0) && text.starts_with(&format!("OK refs q:2/{s2} — 2 references in 2 files; ")) && text.contains("\nS1 function src/orders.rs:1:8 save\n"), "{text}");
    let asked = fix.messages().into_iter().find(|m| m["method"] == "textDocument/references").unwrap();
    let uri = format!("file://{}/src/orders.rs", fix.env.work().display());
    assert_eq!(asked["params"], json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 7}, "context": {"includeDeclaration": false}}));
    assert!(fix.messages().iter().any(|m| m["method"] == "textDocument/didOpen" && m["params"]["textDocument"]["uri"] == uri), "the file was opened first");
    let (code, text) = fix.itse(&["info", "S99"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR info — no stored result holds S99\n"));
    let (code, text) = fix.itse(&["info", &format!("q:1/{s2}")]);
    assert!(
        code == Some(0) && text.starts_with(&format!("OK info q:1/{s2} — 1 match; ")) && text.contains("\nS1 function src/orders.rs:1:8 save\n"),
        "an id of a named result: {text}"
    );
}

#[test]
fn a_qualifier_matches_the_last_container_segment_or_the_path() {
    let Some(fix) = nav() else { return };
    let region = "\nS1 method src/data/region.rs:5:12 Region::save\n";
    let orders = "\nS1 function src/orders.rs:1:8 save\n";
    for (name, kind, want) in [
        ("data::Region.save", None, region),
        ("Region#save", None, region),
        ("Region.sav", None, region),
        ("orders::save", None, orders),
        ("src::orders::save", None, orders),
        ("save", Some("function"), orders),
        ("region", Some("struct"), "\nS1 struct src/data/region.rs:2:12 Region\n"),
    ] {
        let args: Vec<&str> = ["find", name].into_iter().chain(kind.into_iter().flat_map(|k| ["--kind", k])).collect();
        let (code, text) = fix.itse(&args);
        let scope = args[1..].join(" ");
        assert!(code == Some(0) && text.starts_with(&format!("OK find {scope} — 1 match; ")) && text.contains(want), "{args:?}: {text}");
    }
    let (code, text) = fix.itse(&["find", "save", "--kind", "method"]);
    assert!(code == Some(0) && text.starts_with("OK find save --kind method — 1 match; ") && text.contains(region), "a function in a container, not a free one: {text}");
    for name in ["data::save", "x::Region.save", "Other.save"] {
        let (code, text) = fix.itse(&["find", name]);
        assert!(code == Some(1) && text.starts_with(&format!("EMPTY find {name} — 0 matches; ")), "{name}: {text}");
    }
    let (code, text) = fix.itse(&["find", "save", "--kind", "bogus"]);
    assert!(code == Some(2) && text.starts_with("ERROR find — unknown kind bogus; one of file, module, "), "{text}");
}

#[test]
fn zero_matches_is_empty_with_nearest_and_exit_1() {
    let Some(fix) = nav() else { return };
    let (code, text) = fix.itse(&["find", "Other.save"]);
    assert_eq!(code, Some(1), "{text}");
    let [s1, s2] = [1, 2].map(|n| format!("S{n}"));
    assert!(
        text.starts_with(&format!("EMPTY find Other.save — 0 matches; nearest: {s1} Region::save, {s2} save; fixture index cold; q:1\nS1 method src/data/region.rs:5:12 ")),
        "{text}"
    );
    let (code, text) = fix.itse(&["info", &s1]);
    assert_eq!(code, Some(0), "a nearest name is one id away: {text}");
    let want = format!(
        "OK info {s1} — 1 match; fixture index warm; q:2\nS1 method src/data/region.rs:5:12 Region::save\n      5 |     pub fn save(&self) {{}}\n   pub fn save(&self)\n   Saves the region.\n"
    );
    assert!(text.starts_with(&want), "{text}");
    let (code, text) = fix.itse(&["refs", "nothing"]);
    assert_eq!((code, text.as_str()), (Some(1), "EMPTY refs nothing — 0 matches; fixture index warm\n"));
    let (code, text) = fix.itse(&["find", "Regoin"]);
    assert!(code == Some(1) && text.starts_with(&format!("EMPTY find Regoin — 0 matches; nearest: {s1} Region; ")), "a typo the server finds nothing for: {text}");
}

#[test]
fn paths_are_repo_relative_with_the_source_line() {
    let Some(fix) = nav() else { return };
    let (work, sub) = (fix.env.work(), fix.env.work().join("src"));
    let run = |args: &[&str]| {
        let out = fix.env.command(&sub, args).envs(fix.vars.iter().copied()).output().unwrap();
        (out.status.code(), stdout(&out))
    };
    let (code, text) = run(&["outline", "data/region.rs"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(
        text.starts_with("OK outline data/region.rs — 2 symbols; fixture index ")
            && text.contains(
                "\nS1 method src/data/region.rs:5:12 save\n      5 |     pub fn save(&self) {}\nS2 struct src/data/region.rs:2:12 Region\n      2 | pub struct Region;\n"
            ),
        "from a subfolder: {text}"
    );
    for args in [&["find", "data::Region.save"][..], &["info", "data::Region.save"], &["refs", "data::Region.save"]] {
        let (code, text) = run(args);
        assert_eq!(code, Some(0), "{text}");
        assert!(text.contains(" src/data/region.rs:5:12 Region::save\n      5 |     pub fn save(&self) {}\n") && !text.contains(&work.display().to_string()), "{text}");
    }
}

#[test]
fn cli_and_mcp_print_identical_text_for_a_nav_verb() {
    let Some(fix) = nav() else { return };
    assert_eq!(fix.itse(&["status"]).0, Some(0));
    let mut mcp = fix.env.mcp(&fix.vars);
    mcp.handshake("e2e");
    let (_, warm) = mcp.call("problems_symbols", json!({"verb": "find", "args": ["save", "--kind", "function"]}));
    assert!(warm.starts_with("OK find save --kind function — 1 match; fixture index cold; q:1\n"), "the call that starts the server says cold: {warm}");
    // Each call stores its own result, so the ids differ by one.
    for (n, (cli, args)) in
        [(vec!["find", "save"], json!({"verb": "find", "target": "save"})), (vec!["outline", "src/orders.rs"], json!({"verb": "outline", "args": ["src/orders.rs"]}))]
            .into_iter()
            .enumerate()
    {
        let (code, text) = fix.itse(&cli);
        assert_eq!(code, Some(0), "{text}");
        let (is_error, over_mcp) = mcp.call("problems_symbols", args);
        let (a, b) = (format!("; q:{}\n", 2 * n + 2), format!("; q:{}\n", 2 * n + 3));
        assert!(!is_error && text.contains(&a) && over_mcp == text.replacen(&a, &b, 1), "{cli:?}:\n{text}\n{over_mcp}");
    }
}

#[test]
fn problems_is_empty_after_a_no_op_edit() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let lib = fix.env.work().join("src/lib.rs");
    let (code, text) = fix.itse(&["problems"]);
    assert_eq!((code, text.as_str()), (Some(0), "EMPTY problems — 0 errors, 0 warnings; fixture index cold\n"), "the server starts for problems");
    let saves = || fix.messages().iter().filter(|m| m["method"] == "textDocument/didSave").count();
    // Each right after the call before, as an agent edits.
    let edit = |body: String| fs::write(&lib, body).unwrap();
    edit(format!("{LIB}// a comment\n"));
    let (code, text) = fix.itse(&["problems"]);
    assert_eq!((code, text.as_str(), saves()), (Some(0), "EMPTY problems — 0 errors, 0 warnings; fixture index warm\n", 1), "the edit was synced");
    edit(format!("{LIB}FIXTURE_ERROR\n"));
    let (code, text) = fix.itse(&["problems", "src/"]);
    assert_eq!(code, Some(1), "{text}");
    assert_eq!(text, "FAIL problems src/ — 1 error, 0 warnings; fixture index warm; q:1\nE1 error src/lib.rs:3:1 FIXTURE_ERROR found [fixture]\n      3 | FIXTURE_ERROR\n");
    let (_, text) = fix.itse(&["problems", "notes.md"]);
    assert_eq!(text, "EMPTY problems notes.md — 0 errors, 0 warnings\n", "a file no server takes asks none");
    edit(format!("{LIB}// a comment\n"));
    let (code, text) = fix.itse(&["problems"]);
    assert_eq!((code, text.as_str()), (Some(0), "EMPTY problems — 0 errors, 0 warnings; fixture index warm\n"), "the fix cleared it");
}

#[test]
fn a_hook_call_never_starts_a_server() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    assert_eq!(fix.itse(&["status"]).0, Some(0));
    let text = super::cargo::hook_problems(&fix.env, "s1");
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 0 unchanged, 0 fixed\n") && !fix.log.exists(), "{text}");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    let text = super::cargo::hook_problems(&fix.env, "s1");
    assert!(
        text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 0 unchanged, 0 fixed since ") && text.contains("; fixture index warm\n"),
        "a running one is asked: {text}"
    );
}

#[test]
fn an_unsettled_call_does_not_advance_the_baseline() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "index_ms": 5000}), "", false) else { return };
    let started = Instant::now();
    let fast = [("ITSE_TEST_SETTLE_MS", "1000")];
    let (_, text) = fix.itse_env(&["problems", "--new"], &fast);
    assert!(text.contains("\nnot settled: the baseline is not advanced\nfixture not settled after 1 s: its diagnostics may be behind the disk\n"), "{text}");
    fs::write(fix.env.work().join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let (code, text) = fix.itse_env(&["problems", "--new"], &fast);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; fixture index cold (Indexing 0/3 (0%)); q:"), "{text}");
    assert!(text.contains("\nE1 error src/lib.rs:3:1 FIXTURE_ERROR found [fixture]\n") && text.contains("\nnot settled: the baseline is not advanced\n"), "{text}");
    thread::sleep(Duration::from_millis(5200).saturating_sub(started.elapsed()));
    let (_, text) = fix.itse(&["problems", "--new"]);
    assert!(text.starts_with("FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; fixture index warm; q:"), "still new once settled: {text}");
    let (code, text) = fix.itse(&["problems", "--new"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "), "{text}");
}

#[test]
fn a_server_not_ready_keeps_its_part_of_the_baseline_and_its_problems_are_not_fixed() {
    let env = Env::new();
    if !repo(&env, &[("src/lib.rs", "pub fn f() {}\nFIXTURE_ERROR\nPATH_ERROR\n"), ("src/a.txt", "PATH_ERROR\n")]) {
        return eprintln!("skipped: needs git");
    }
    // `fixture` is always there; `onpath` is a path install, ready only while the caller's PATH has it.
    let server = |id: &str, bin: &str, filetypes: &[&str], marker: &str| json!({"id": id, "protocol": "lsp", "command": [bin, "lsp"], "filetypes": filetypes, "env": {"ITSE_FIXTURE_SCRIPT": json!({"marker": marker}).to_string()}});
    let package = json!({"purl": "pkg:generic/x/fxls", "install": "path", "bin": {"itse-fixture": "itse-fixture"}});
    let servers = [server("fixture", FIXTURE, &["rs"], "FIXTURE_ERROR"), server("onpath", "${pkg:fxls}/itse-fixture", &["rs", "txt"], "PATH_ERROR")];
    let ext = env.dir.join("two.json");
    fs::write(&ext, json!({"itse_ext": 1, "id": "two", "packages": {"fxls": package}, "servers": servers}).to_string()).unwrap();
    let ready = format!("{}:{}", Path::new(FIXTURE).parent().unwrap().display(), std::env::var("PATH").unwrap_or_default());
    let itse = |path: &str, args: &[&str]| {
        let out = env.command(&env.work(), args).env("PATH", path).envs(IDLE.iter().copied()).output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    };
    for args in [&["ext", "add", ext.to_str().unwrap()][..], &["use", "two"]] {
        assert_eq!(itse(&ready, args).0, Some(0), "{args:?}");
    }
    let (code, text) = itse(&ready, &["problems", "src/lib.rs", "--new"]);
    assert!(code == Some(1) && text.starts_with("FAIL problems src/lib.rs — 2 new errors, 0 new warnings; 0 unchanged, 0 fixed; "), "{text}");
    // Gone from PATH: what it pushed stays in the baseline, not fixed, and the other server's part advances.
    let (code, text) = itse("/usr/bin:/bin", &["problems", "src/lib.rs", "--new"]);
    assert!(code == Some(0) && text.starts_with("EMPTY problems src/lib.rs — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "), "{text}");
    assert!(text.contains("\nonpath not ready: its diagnostics are not included\n") && !text.contains("not settled"), "{text}");
    let (code, text) = itse(&ready, &["problems", "src/lib.rs", "--new"]);
    assert!(code == Some(0) && text.starts_with("EMPTY problems src/lib.rs — 0 new errors, 0 new warnings; 2 unchanged, 0 fixed since "), "back, nothing new: {text}");
    // The only server for a file not ready: the same, and a query that needs it says what is missing.
    let (code, text) = itse(&ready, &["problems", "src/a.txt", "--new"]);
    assert!(code == Some(1) && text.starts_with("FAIL problems src/a.txt — 1 new error, 0 new warnings; 0 unchanged, 0 fixed since "), "{text}");
    let (code, text) = itse("/usr/bin:/bin", &["problems", "src/a.txt", "--new"]);
    assert!(code == Some(0) && text.starts_with("EMPTY problems src/a.txt — 0 new errors, 0 new warnings; 0 unchanged, 0 fixed since "), "{text}");
    assert!(text.contains("\nonpath not ready: its diagnostics are not included\n"), "{text}");
    let (_, text) = itse("/usr/bin:/bin", &["outline", "src/a.txt"]);
    assert!(text.starts_with("UNREADY outline src/a.txt — onpath not ready\n") && text.contains("itse-fixture not on PATH"), "{text}");
    let (code, text) = itse(&ready, &["problems", "src/a.txt", "--new"]);
    assert!(code == Some(0) && text.starts_with("EMPTY problems src/a.txt — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "), "back: {text}");
}

#[test]
fn a_changed_launch_shows_in_status_until_the_server_is_stopped() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let layer = fix.env.dir.join("config/itse/workspaces/work.json");
    let mut dev: Value = serde_json::from_slice(&fs::read(&layer).unwrap()).unwrap();
    dev["servers"] = json!({"fixture": {"settings": {"fixture": {"level": 3}}}});
    fs::write(&layer, dev.to_string()).unwrap();
    assert!(fix.row().contains("; config changed; 'itse stop lsp:fixture' to apply"), "{}", fix.row());
    let (_, text) = fix.itse(&["find", "save"]);
    assert!(text.contains("\nconfig changed; 'itse stop lsp:fixture' to apply\n"), "{text}");
    assert_eq!(fix.itse(&["stop", "lsp:fixture"]).0, Some(0));
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    assert!(!fix.row().contains("config changed"), "{}", fix.row());
    let answers: Vec<Value> = fix.messages().into_iter().filter(|m| m["id"] == "cfg-1").map(|m| m["result"].clone()).collect();
    assert_eq!(answers, [json!([{"level": 2}]), json!([{"level": 3}])], "the restart got the new settings");
}

#[test]
fn a_garbage_or_oversized_frame_leaves_the_server_usable() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "garbage-frame", false) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let row = fix.row();
    assert!(row.starts_with("up ") && row.contains(" skipped: not JSON: "), "the frame went to the server log: {row}");
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "oversize-frame", false) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR find — lsp:fixture: response_too_large (-32603)\n"));
    let (code, text) = fix.itse(&["outline", "src/lib.rs"]);
    assert!(code == Some(0) && text.starts_with("OK outline src/lib.rs — 2 symbols; "), "the stream stayed in sync: {text}");
}

#[test]
fn a_checkout_that_leaves_the_tree_clean_is_told_to_the_server() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    for args in [&["checkout", "-q", "-b", "bad"][..], &["commit", "-qam", "bad"]] {
        fs::write(work.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
        assert!(git(&fix.env, &work, args), "{args:?}");
    }
    let (code, text) = fix.itse(&["problems"]);
    assert!(code == Some(1) && text.contains("\nE1 error src/lib.rs:3:1 FIXTURE_ERROR found [fixture]\n"), "the diff between the two HEADs: {text}");
    assert!(git(&fix.env, &work, &["checkout", "-q", "main"]));
    let (code, text) = fix.itse(&["problems"]);
    assert_eq!((code, text.as_str()), (Some(0), "EMPTY problems — 0 errors, 0 warnings; fixture index warm\n"), "and back");
}

#[test]
fn a_hook_call_keeps_the_baseline_of_a_server_not_running() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let lib = fix.env.work().join("src/lib.rs");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let first = |text: String| text.lines().next().unwrap_or_default().to_string();
    assert_eq!(first(super::cargo::hook_problems(&fix.env, "s1")), "FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; fixture index warm; q:1");
    assert_eq!(fix.itse(&["stop", "lsp:fixture"]).0, Some(0));
    let text = super::cargo::hook_problems(&fix.env, "s1");
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 0 unchanged, 0 fixed since "), "not asked is not fixed: {text}");
    assert!(text.ends_with("\nfixture not running: its diagnostics are not asked\n"), "{text}");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let text = first(super::cargo::hook_problems(&fix.env, "s1"));
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 1 unchanged, 0 fixed since "), "back, and not new: {text}");
}

#[test]
fn what_a_server_started_in_a_group_of_its_own_ends_with_it() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "spawn-child", false) else { return };
    let child = |fix: &Fix| {
        let path = fix.log.with_extension("log.child");
        assert!(wait_until(Duration::from_secs(5), || fs::read_to_string(&path).is_ok_and(|p| !p.is_empty())));
        fs::read_to_string(&path).unwrap().trim().parse::<i32>().unwrap()
    };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let (server, sleeper) = (fix.server_pid().unwrap(), child(&fix));
    assert!(alive(sleeper));
    signal(server, "KILL");
    assert!(wait_until(Duration::from_secs(5), || !alive(sleeper)), "a crash takes it along");
    // The daemon finds the crash on its next tick, and holds the slot while it kills the group.
    assert!(wait_until(Duration::from_secs(3), || fix.row().starts_with("crashed ")), "{}", fix.row());
    let (code, text) = fix.itse(&["find", "save"]);
    assert!(code == Some(0) && text.contains("\nfixture restarted after a crash (killed by signal 9 (out of memory, or killed))\n"), "{text}");
    let sleeper = child(&fix);
    assert!(alive(sleeper));
    assert_eq!(fix.itse(&["stop", "lsp:fixture"]).0, Some(0));
    assert!(wait_until(Duration::from_secs(3), || !alive(sleeper)), "and so does a stop");
}

#[test]
fn the_daemon_waits_for_an_idle_stop_to_say_goodbye() {
    let Some(fix) = Fix::with(json!({"symbols": symbols()}), "", false, &[("ITSE_SERVER_IDLE_S", "2"), ("ITSE_DAEMON_IDLE_S", "2")]) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let daemon = fix.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    assert!(wait_until(Duration::from_secs(15), || !alive(daemon)), "both idle after 2 s");
    let methods = fix.methods();
    assert_eq!(methods[methods.len() - 2..], ["shutdown", "exit"], "{methods:?}");
}

#[test]
fn concurrent_first_queries_all_say_the_index_is_cold() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "slow_ms": 1500}), "slow", false) else { return };
    assert_eq!(fix.itse(&["status"]).0, Some(0));
    let finds: Vec<_> = (0..3).map(|_| fix.env.command(&fix.env.work(), &["find", "save"]).envs(fix.vars.iter().copied()).stdout(Stdio::piped()).spawn().unwrap()).collect();
    for find in finds {
        let text = stdout(&find.wait_with_output().unwrap());
        assert!(text.starts_with("OK find save — 1 match; fixture index cold"), "each waited for the start: {text}");
    }
    assert_eq!(fix.methods().iter().filter(|m| *m == "initialize").count(), 1);
}

#[test]
fn outline_takes_a_path_from_the_callers_folder_first() {
    let symbols = json!([{"name": "save", "kind": 12, "path": "src/lib.rs", "line": 1, "character": 7}, {"name": "inner", "kind": 12, "path": "crates/foo/src/lib.rs", "line": 0, "character": 7}]);
    let Some(fix) = Fix::new(json!({"symbols": symbols}), "", false) else { return };
    let foo = fix.env.work().join("crates/foo");
    fs::create_dir_all(foo.join("src")).unwrap();
    fs::write(foo.join("src/lib.rs"), "pub fn inner() {}\n").unwrap();
    let out = fix.env.command(&foo, &["outline", "src/lib.rs"]).envs(fix.vars.iter().copied()).output().unwrap();
    let text = stdout(&out);
    assert!(out.status.success() && text.contains("\nS1 function crates/foo/src/lib.rs:1:8 inner\n") && !text.contains(" save"), "{text}");
    let out = fix.env.command(&foo, &["outline", "../../src/lib.rs"]).envs(fix.vars.iter().copied()).output().unwrap();
    let text = stdout(&out);
    assert!(out.status.success() && text.contains("\nS1 function src/lib.rs:2:8 save\n"), "one form of each path: {text}");
    let opened = fix.messages().into_iter().filter(|m| m["method"] == "textDocument/didOpen").map(|m| m["params"]["textDocument"]["uri"].clone()).collect::<Vec<_>>();
    assert!(opened.iter().all(|u| !u.as_str().unwrap_or("").contains("/../")), "{opened:?}");
}

#[test]
fn an_unused_extension_leaves_no_server_running() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    assert_eq!(fix.itse(&["find", "save"]).0, Some(0));
    let pid = fix.server_pid().unwrap();
    assert_eq!(fix.itse(&["unuse", "fix"]).0, Some(0));
    let (_, text) = fix.itse(&["find", "save"]);
    assert!(text.starts_with("EMPTY find save — no language server in use"), "{text}");
    assert!(wait_until(Duration::from_secs(3), || !alive(pid) && fix.server_pid().is_none()), "stopped once its extension is no longer in use");
}

#[test]
fn a_save_the_server_checks_on_is_sent_again_and_an_unheard_one_holds_the_baseline() {
    // Saves 1 and 2 are heard; 3 is dropped and its second send heard; 5 and its second send are dropped.
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "check": true, "drop_saves": [3, 5, 6]}), "", false) else { return };
    let lib = fix.env.work().join("src/lib.rs");
    let edit = |bad: bool| fs::write(&lib, if bad { format!("{LIB}FIXTURE_ERROR\n") } else { format!("{LIB}// fine\n") }).unwrap();
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    edit(true);
    let (code, text) = fix.itse(&["problems", "--new"]);
    assert!(code == Some(1) && text.starts_with("FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; "), "{text}");
    edit(false);
    let (_, text) = fix.itse(&["problems", "--new"]);
    assert!(text.starts_with("EMPTY problems — 0 new errors, 0 new warnings; 0 unchanged, 1 fixed since "), "{text}");
    edit(true);
    let started = Instant::now();
    let (code, text) = fix.itse(&["problems", "--new"]);
    assert!(code == Some(1) && text.starts_with("FAIL problems — 1 new error, 0 new warnings; 0 unchanged, 0 fixed since "), "sent again: {text}");
    assert!(started.elapsed() >= Duration::from_millis(1500), "{:?}", started.elapsed());
    edit(false);
    let (_, text) = fix.itse(&["problems", "--new"]);
    assert!(text.starts_with("FAIL problems — ") && text.contains("\nE1 error src/lib.rs:3:1 FIXTURE_ERROR found [fixture]\n"), "never heard: the old push stays: {text}");
    assert!(
        text.ends_with("\nnot settled: the baseline is not advanced\nfixture began no check after the last save, sent twice: its diagnostics may be behind the disk\n"),
        "{text}"
    );
    let (_, text) = fix.itse(&["problems", "--new"]);
    assert!(text.contains("\nnot settled: the baseline is not advanced\n"), "still unheard, so still held: {text}");
    assert_eq!(fix.methods().iter().filter(|m| *m == "textDocument/didSave").count(), 6);
}

#[test]
fn a_server_whose_package_runs_project_code_waits_for_trust_too() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let server = json!({"id": "pkgfix", "protocol": "lsp", "command": [FIXTURE, "lsp"], "filetypes": ["rs"], "env": {"PKG": "${pkg:pkgls}", "ITSE_FIXTURE_LOG": fix.log}});
    let ext = json!({"itse_ext": 1, "id": "pkg", "packages": {"pkgls": {"purl": "pkg:generic/x/pkgls", "install": "path", "executes_project_code": true}}, "servers": [server]});
    let path = fix.env.dir.join("pkg.json");
    fs::write(&path, ext.to_string()).unwrap();
    for args in [&["unuse", "fix"][..], &["ext", "add", path.to_str().unwrap()], &["use", "pkg"]] {
        assert_eq!(fix.itse(args).0, Some(0), "{args:?}");
    }
    let (code, text) = fix.itse(&["find", "save"]);
    assert!(code == Some(3) && text.starts_with("UNTRUSTED find save — pkgfix runs project code\n") && text.contains("next (human): itse trust"), "{text}");
    let (code, text) = fix.itse(&["problems", "--new"]);
    assert!(code == Some(3) && text.starts_with("UNTRUSTED problems — pkgfix runs project code; "), "{text}");
    assert!(!fix.log.exists(), "nothing started");
}

#[test]
fn nested_symbols_links_and_inline_modules_take_the_paths_real_servers_answer_with() {
    let nest = "pub struct Region;\nimpl Region {\n    pub fn save(&self) {}\n    // local\n}\npub mod inner {\n    pub fn deep_fn() {}\n}\n";
    let nested = json!([{"name": "Region", "kind": 23, "path": "src/nest.rs", "line": 0, "character": 11},
        {"name": "save", "kind": 6, "container": "Region", "path": "src/nest.rs", "line": 2, "character": 11},
        {"name": "local", "kind": 13, "container": "save", "path": "src/nest.rs", "line": 3, "character": 7},
        {"name": "inner", "kind": 2, "path": "src/nest.rs", "line": 5, "character": 8},
        {"name": "deep_fn", "kind": 12, "module": "inner", "path": "src/nest.rs", "line": 6, "character": 11}]);
    let definition = json!({"path": "src/nest.rs", "line": 0, "character": 11});
    let Some(fix) = Fix::new(json!({"symbols": nested, "hierarchical": true, "links": true, "definition": definition, "hover": "a thing"}), "", false) else { return };
    fs::write(fix.env.work().join("src/nest.rs"), nest).unwrap();
    let (code, text) = fix.itse(&["outline", "src/nest.rs"]);
    let items = "\nS1 struct src/nest.rs:1:12 Region\n      1 | pub struct Region;\nS2 method src/nest.rs:3:12   save\n      3 |     pub fn save(&self) {}\
        \nS3 module src/nest.rs:6:9 inner\n      6 | pub mod inner {\nS4 function src/nest.rs:7:12   deep_fn\n";
    assert!(code == Some(0) && text.starts_with("OK outline src/nest.rs — 4 symbols; ") && text.contains(items) && !text.contains("local"), "two levels: {text}");
    let (code, text) = fix.itse(&["find", "inner::deep_fn"]);
    assert!(code == Some(0) && text.contains("\nS1 function src/nest.rs:7:12 deep_fn\n"), "the module from the file's outline: {text}");
    let (code, text) = fix.itse(&["info", "inner::deep_fn"]);
    assert!(code == Some(0) && text.contains("\nS1 function src/nest.rs:1:12 deep_fn\n      1 | pub struct Region;\n   a thing\n"), "at the LocationLink's target: {text}");
    // A server without a definition keeps the symbol where it is, with its hover; one without
    // workspace symbols is not asked for them.
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "hover": "a thing", "caps_off": ["definitionProvider"]}), "", false) else { return };
    let (code, text) = fix.itse(&["info", "save"]);
    assert!(code == Some(0) && text.contains("\nS1 function src/lib.rs:2:8 orders::save\n      2 | pub fn save() {}\n   a thing\n"), "{text}");
    assert!(!fix.methods().iter().any(|m| m == "textDocument/definition"));
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "caps_off": ["workspaceSymbolProvider"]}), "", false) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert!(code == Some(1) && text.starts_with("EMPTY find save — 0 matches; "), "{text}");
    assert!(!fix.methods().iter().any(|m| m == "workspace/symbol"));
}

#[test]
fn a_server_logs_its_stderr_and_gets_no_itse_or_claude_var_but_its_marker() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let (code, text) = fix.itse_env(&["find", "save"], &[("CLAUDE_CODE_ENTRYPOINT", "cli"), ("ITSE_SESSION", "s1"), ("AI_AGENT", "x")]);
    assert_eq!(code, Some(0), "{text}");
    let environ = fs::read(format!("/proc/{}/environ", fix.server_pid().unwrap())).unwrap();
    let vars: Vec<String> = environ.split(|&b| b == 0).filter_map(|v| std::str::from_utf8(v).ok()?.split('=').next().map(String::from)).collect();
    let ours: Vec<&String> = vars.iter().filter(|v| v.starts_with("ITSE_") || v.starts_with("CLAUDE_") || *v == "AI_AGENT").collect();
    assert_eq!(ours, ["ITSE_FIXTURE_LOG", "ITSE_FIXTURE_MODE", "ITSE_FIXTURE_SCRIPT", "ITSE_SERVER"], "the declared env and the marker only: {vars:?}");
    let log = fs::read_to_string(fix.env.daemons()[0].0.join("servers/fixture.log")).unwrap();
    assert!(log.starts_with("itse-fixture lsp: started\n") && log.contains(" window/logMessage: fixture indexing\n"), "{log}");
}

#[test]
fn a_server_that_ends_right_after_initialize_is_a_crash_and_starts_again() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "crash-after-init", false) else { return };
    let (code, text) = fix.itse(&["find", "save"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.starts_with("ERROR find — server_crash: lsp:fixture ended during the call (eof); the next call starts it again"), "{text}");
    assert!(wait_until(Duration::from_secs(3), || fix.row().starts_with("crashed        lsp:fixture          exit 3")), "{}", fix.row());
    assert_eq!(fix.itse(&["find", "save"]).0, Some(2));
    assert_eq!(fix.methods().iter().filter(|m| *m == "initialize").count(), 2, "the next call started it again");
    let (_, text) = fix.itse(&["stop", "fixture"]);
    assert!(
        text.starts_with("STOPPED stop lsp:fixture — stopped\n") || text.starts_with("ERROR stop — lsp:fixture is not running (crashed: exit 3"),
        "a bare server id names its handle: {text}"
    );
}

#[test]
fn an_empty_answer_on_a_loading_index_waits_for_it_and_says_to_retry() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "index_ms": 4500}), "", false) else { return };
    let started = Instant::now();
    let (code, text) = fix.itse(&["find", "nothing"]);
    assert!(started.elapsed() >= Duration::from_millis(4500), "past the 3 s a found name waits: {:?}", started.elapsed());
    assert_eq!((code, text.as_str()), (Some(1), "EMPTY find nothing — 0 matches; fixture index cold\nindex cold: an empty answer may change; retry\n"), "settled, asked again");
}
