//! Hosted MCP servers end to end against `itse-fixture mcp`: the version check, paged tool lists,
//! exposed tools listed and called through `itse mcp`, the first listing's wait, crashes, trust,
//! and the server's own requests.

use super::*;

/// A workspace that uses the extension `fix`, whose one server is the fixture MCP server with
/// this env, its messages logged; `code` marks it as running project code.
struct Hosted {
    env: Env,
    log: PathBuf,
}

impl Hosted {
    fn new(vars: &[(&str, &str)], code: bool) -> Self {
        Self::declared(vars, code, json!({}))
    }

    /// The same, with the server's declaration given the keys of `more`.
    fn declared(vars: &[(&str, &str)], code: bool, more: Value) -> Self {
        let env = Env::new();
        let log = env.dir.join("fixture.log");
        let mut server_env: serde_json::Map<String, Value> = vars.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        server_env.insert("ITSE_FIXTURE_LOG".into(), json!(log));
        let mut server = json!({"id": "fixture", "protocol": "mcp", "transport": "stdio", "command": [FIXTURE, "mcp"], "env": server_env, "executes_project_code": code});
        server.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
        let ext = env.dir.join("fix.json");
        fs::write(&ext, json!({"itse_ext": 1, "id": "fix", "servers": [server]}).to_string()).unwrap();
        let hosted = Self { env, log };
        for args in [&["ext", "add", ext.to_str().unwrap()][..], &["use", "fix"]] {
            let (code, text) = hosted.itse(args);
            assert_eq!(code, Some(0), "{args:?}: {text}");
        }
        hosted
    }

    /// Set the developer layer's `servers.fixture`, as a developer edits it.
    fn settings(&self, settings: Value) -> &Self {
        let dir = self.env.dir.join("config/itse/workspaces");
        let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let mut layer: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        layer["servers"] = json!({"fixture": settings});
        fs::write(&path, layer.to_string()).unwrap();
        self
    }

    /// `itse ARGS` in the work folder, servers kept for a minute: (exit code, stdout and stderr).
    fn itse(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.env.command(&self.env.work(), args).env("ITSE_SERVER_IDLE_S", "60").output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    }

    fn mcp(&self) -> Mcp {
        let mut mcp = self.env.mcp(&[("ITSE_SERVER_IDLE_S", "60")]);
        mcp.handshake("e2e");
        mcp
    }

    /// The messages the fixture got, from every start.
    fn messages(&self) -> Vec<Value> {
        fs::read_to_string(&self.log).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn starts(&self) -> usize {
        self.messages().iter().filter(|m| m["method"] == "initialize").count()
    }
}

const CORE: [&str; 3] = ["test_build_run", "problems_symbols", "show_result"];

#[test]
fn a_hosted_server_must_answer_a_known_version() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_VERSION", "2024-01-01")], false);
    let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.starts_with("ERROR ext — mcp:fixture: start_failed: ") && text.contains("at initialize (mcp_version_unsupported: it answered '2024-01-01'; itse speaks 2024-11-05, "),
        "{text}"
    );
    assert_eq!(hosted.messages()[0]["params"]["protocolVersion"], "2025-06-18", "itse asks for 2025-06-18");
    for version in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
        let hosted = Hosted::new(&[("ITSE_FIXTURE_VERSION", version)], false);
        let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
        assert!(code == Some(0) && text.starts_with("OK ext tools fixture — 2 tools; not exposed;"), "{version}: {text}");
        assert!(hosted.messages().iter().any(|m| m["method"] == "notifications/initialized"), "{version}");
    }
}

#[test]
fn a_hosted_servers_tool_list_follows_next_cursor_up_to_200_tools() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_TOOLS", "11"), ("ITSE_FIXTURE_PAGE", "4")], false);
    let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK ext tools fixture — 14 tools; "), "4 pages: {text}");
    assert!(text.contains("\nfixture__echo — Answers its text.\n") && text.contains("\nfixture__tool_11 — Tool 11 answers its text.\n"), "{text}");
    let lists = hosted.messages().iter().filter(|m| m["method"] == "tools/list").count();
    assert_eq!(lists, 4);
    let (_, again) = hosted.itse(&["ext", "tools", "fixture"]);
    assert_eq!((again, hosted.messages().iter().filter(|m| m["method"] == "tools/list").count()), (text, 4), "the second listing is the cached one");
    let many = Hosted::new(&[("ITSE_FIXTURE_TOOLS", "230"), ("ITSE_FIXTURE_PAGE", "50")], false);
    let (_, text) = many.itse(&["ext", "tools", "fixture"]);
    assert!(text.starts_with("OK ext tools fixture — 200 tools; "), "{text}");
}

#[test]
fn an_exposed_tool_is_called_through_itse_mcp() {
    let hosted = Hosted::new(&[], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names(), [&CORE[..], &["fixture__echo", "fixture__slow"]].concat(), "the server starts for the first listing");
    assert_eq!(mcp.call("fixture__echo", json!({"text": "hi from the harness"})), (false, "hi from the harness".into()));
    assert_eq!(mcp.call("fixture__nope", json!({})), (true, "ERROR fixture__nope — no such tool\n".into()));
    let (is_error, status) = mcp.call("problems_symbols", json!({"verb": "status"}));
    assert!(!is_error && status.contains("\nmcp tools: 5 (~"), "{status}");
    assert!(status.lines().any(|l| l.starts_with("up ") && l.contains("mcp:fixture") && l.contains("pid ")), "{status}");
    assert_eq!(hosted.starts(), 1);
    let calls: Vec<Value> = hosted.env.usage().into_iter().filter(|r| r["verb"] == "ext_tool").collect();
    assert_eq!(calls.len(), 1, "the exposed call is counted as a class");
    assert_eq!((calls[0]["outcome"].as_str(), calls[0]["ext"].is_null(), calls[0]["via"].as_str()), (Some("ok"), true, Some("mcp")));
    let log = fs::read_to_string(hosted.env.dir.join("state/itse/usage.jsonl")).unwrap();
    assert!(!log.contains("hi from the harness") && !log.contains("fixture"), "no argument, server or tool name: {log}");
    hosted.env.usage().iter().for_each(check_record);
    let (code, text) = hosted.itse(&["stop", "mcp:fixture"]);
    assert_eq!((code, text.as_str()), (Some(0), "STOPPED stop mcp:fixture — stopped\n"));
    assert_eq!(hosted.messages().last(), Some(&json!({"eof": true})), "stopped by the end of its stdin, before any signal");
}

#[test]
fn a_tool_result_passes_through_as_the_server_sent_it() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "bare")], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert!(mcp.tool_names().contains(&"fixture__bare".to_string()));
    assert_eq!(mcp.rpc("tools/call", json!({"name": "fixture__bare"})), json!({}), "no content is added");
    let call = hosted.messages().into_iter().rfind(|m| m["method"] == "tools/call").unwrap();
    assert_eq!(call["params"]["arguments"], json!({}), "no arguments are an empty object, not null");
    let result = mcp.rpc("tools/call", json!({"name": "fixture__bare", "arguments": {"result": null}}));
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(result["content"][0]["text"], "ERROR fixture__bare — mcp:fixture: tools/call answered null, not a result object\n");
}

#[test]
fn a_server_that_pages_forever_ends_its_listing() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "endless")], false);
    let started = Instant::now();
    let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
    assert!(code == Some(0) && text.starts_with("OK ext tools fixture — 2 tools; "), "{text}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(hosted.messages().iter().filter(|m| m["method"] == "tools/list").count(), 2, "an empty page ends it");
}

#[test]
fn a_late_first_listing_that_fails_sends_no_list_changed() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_INIT_MS", "11500"), ("ITSE_FIXTURE_VERSION", "1999-01-01")], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names(), CORE, "late: the core tools only");
    let crashed = |s: &str| s.lines().any(|l| l.starts_with("crashed ") && l.contains("mcp:fixture"));
    assert!(wait_until(Duration::from_secs(10), || crashed(&hosted.itse(&["status"]).1)), "{}", hosted.itse(&["status"]).1);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(mcp.rpc("ping", json!({})), json!({}), "the next message is the ping's answer, not list_changed");
}

#[test]
fn an_exposed_tool_whose_cached_list_is_gone_is_listed_again_for_its_call() {
    let hosted = Hosted::new(&[], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names().len(), 5);
    let cache = hosted.env.daemons()[0].0.join("mcp/fixture.json");
    fs::remove_file(&cache).unwrap();
    assert_eq!(mcp.call("fixture__echo", json!({"text": "still here"})), (false, "still here".into()), "a stale list doesn't make its tools unknown");
    assert!(cache.exists());
    assert_eq!((hosted.starts(), hosted.messages().iter().filter(|m| m["method"] == "tools/list").count()), (1, 2));
}

#[test]
fn a_server_started_again_lists_its_tools_again() {
    let hosted = Hosted::new(&[], false);
    hosted.settings(json!({"expose": true}));
    let lists = || hosted.messages().iter().filter(|m| m["method"] == "tools/list").count();
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names().len(), 5);
    assert_eq!(hosted.itse(&["stop", "mcp:fixture"]).0, Some(0));
    assert_eq!(mcp.call("fixture__echo", json!({"text": "again"})), (false, "again".into()));
    assert_eq!((hosted.starts(), lists()), (2, 1), "the call starts it from the cached list");
    assert_eq!(mcp.tool_names().len(), 5);
    assert_eq!(lists(), 2, "the next listing asks the new start, as its script may have changed");
    assert_eq!(mcp.tool_names().len(), 5);
    assert_eq!(lists(), 2, "once per start");
}

#[test]
fn a_first_listing_waits_10_s_then_sends_list_changed() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_INIT_MS", "11500")], false);
    hosted.settings(json!({"expose": true}));
    let (_, status) = hosted.itse(&["status"]);
    assert!(status.contains("\nfixture: tools not listed yet (first start)\n"), "{status}");
    let mut mcp = hosted.mcp();
    let started = Instant::now();
    assert_eq!(mcp.tool_names(), CORE, "late: the core tools only");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(9800) && waited < Duration::from_millis(11500), "{waited:?}");
    assert_eq!(mcp.recv(), json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}), "once the list is cached");
    assert_eq!(mcp.tool_names(), [&CORE[..], &["fixture__echo", "fixture__slow"]].concat());
    assert_eq!(hosted.starts(), 1);
    drop(mcp);
    let mut next = hosted.mcp();
    let started = Instant::now();
    assert_eq!(next.tool_names().len(), 5);
    assert!(started.elapsed() < Duration::from_secs(2), "a cached list waits for nothing");
}

#[test]
fn a_crash_mid_call_fails_the_call_and_the_next_call_restarts_the_server() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "crash-in-call")], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names().len(), 5);
    let (is_error, text) = mcp.call("fixture__slow", json!({"ms": 100}));
    assert!(is_error && text.starts_with("ERROR fixture__slow — server_crash: mcp:fixture ended during the call (eof); the next call starts it again"), "{text}");
    let result = mcp.rpc("tools/call", json!({"name": "fixture__echo", "arguments": {"text": "back"}}));
    assert_eq!(result["content"][0]["text"], "back");
    assert_eq!(result["content"][1]["text"], "fixture restarted after a crash (exit 3)", "{result}");
    assert_eq!(hosted.starts(), 2);
}

#[test]
fn an_untrusted_project_code_mcp_server_is_not_started() {
    let hosted = Hosted::new(&[], true);
    hosted.settings(json!({"expose": true}));
    let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED ext tools fixture — fixture runs project code\n") && text.contains("next (human): itse trust"), "{text}");
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names(), CORE);
    assert_eq!(mcp.call("fixture__echo", json!({"text": "x"})), (true, "ERROR fixture__echo — no such tool\n".into()), "not listed, so not callable");
    let (_, status) = hosted.itse(&["status"]);
    assert!(status.contains("\nfixture: not started: runs project code; next (human): itse trust\n"), "{status}");
    assert_eq!(hosted.starts(), 0, "never spawned");
    assert!(!hosted.log.exists());
}

#[test]
fn the_allowlist_and_the_list_cap_shape_the_exposed_tools() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_TOOLS", "20")], false);
    hosted.settings(json!({"expose": true, "tools": ["tool-3", "echo"]}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names(), [&CORE[..], &["fixture__echo", "fixture__tool_3"]].concat());
    let list = mcp.rpc("tools/list", json!({}));
    let tool3 = list["tools"].as_array().unwrap().iter().find(|t| t["name"] == "fixture__tool_3").unwrap().clone();
    assert_eq!(tool3["description"], "Tool 3 answers its text.", "the first sentence");
    drop(mcp);
    hosted.settings(json!({"expose": true}));
    let (_, status) = hosted.itse(&["status"]);
    assert!(status.contains("\nfixture: 1 tool skipped, named over 64 chars as fixture__<tool>\n"), "{status}");
    let dropped =
        status.lines().find(|l| l.starts_with("fixture: ") && l.contains(" tools dropped (max_list_bytes 1200 B); set tools: [...]")).unwrap_or_else(|| panic!("{status}"));
    assert!(dropped.contains(" of 22 tools"), "{dropped}");
    // A dropped tool isn't in the list, so a call to it is no call.
    let mut mcp = hosted.mcp();
    let names = mcp.tool_names();
    assert!(!names.contains(&"fixture__tool_20".to_string()), "{names:?}");
    assert_eq!(mcp.call("fixture__tool_20", json!({"text": "x"})), (true, "ERROR fixture__tool_20 — no such tool\n".into()));
    drop(mcp);
    // A share past what the whole list holds meets the list cap.
    hosted.settings(json!({"expose": true, "max_list_bytes": 100_000}));
    let (_, status) = hosted.itse(&["status"]);
    assert!(status.lines().any(|l| l.starts_with("fixture: ") && l.contains(" tools dropped (list cap 3,600 B); set tools: [...]")), "{status}");
    let mut mcp = hosted.mcp();
    let list = mcp.rpc("tools/list", json!({}));
    assert!(list["tools"].as_array().unwrap().len() > 5 && list.to_string().len() <= 3600, "{} B", list.to_string().len());
}

#[test]
fn an_exposed_tool_keeps_the_servers_hints_and_its_timeout_s() {
    let hosted = Hosted::declared(&[], false, json!({"timeout_s": 1}));
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    let list = mcp.rpc("tools/list", json!({}));
    let echo = list["tools"].as_array().unwrap().iter().find(|t| t["name"] == "fixture__echo").unwrap().clone();
    assert_eq!(echo["annotations"], json!({"readOnlyHint": true}), "{echo}");
    assert!(echo.get("title").is_none(), "{echo}");
    let started = Instant::now();
    assert_eq!(mcp.call("fixture__slow", json!({"ms": 5000})), (true, "ERROR fixture__slow — mcp:fixture: timeout: no answer in 1 s\n".into()));
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!(mcp.call("fixture__echo", json!({"text": "still up"})), (false, "still up".into()));
    let timed_out = hosted.env.usage().into_iter().find(|r| r["verb"] == "ext_tool" && r["outcome"] == "timeout");
    assert!(timed_out.is_some(), "{:?}", hosted.env.usage());
}

#[test]
fn list_changed_from_the_server_re_lists_its_tools() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "grow")], false);
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.mcp();
    assert_eq!(mcp.tool_names().len(), 5);
    assert_eq!(mcp.call("fixture__echo", json!({"text": "grow"})), (false, "grow".into()));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(mcp.call("fixture__echo", json!({"text": "still"})), (false, "still".into()), "callable while its list is asked again");
    assert!(wait_until(Duration::from_secs(2), || mcp.tool_names().contains(&"fixture__grown".to_string())), "listed again");
    assert_eq!(mcp.call("fixture__grown", json!({"text": "new"})), (false, "new".into()));
    assert_eq!(hosted.starts(), 1);
}

#[test]
fn a_list_changed_from_a_server_since_stopped_starts_nothing() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "grow")], false);
    hosted.settings(json!({"expose": true}));
    hosted.itse(&["daemon", "stop"]);
    let mut mcp = hosted.env.mcp(&[("ITSE_SERVER_IDLE_S", "1")]);
    mcp.handshake("e2e");
    assert_eq!(mcp.tool_names().len(), 5);
    assert_eq!(mcp.call("fixture__echo", json!({"text": "grow"})), (false, "grow".into()));
    let stopped = |s: &str| s.lines().any(|l| l.starts_with("stopped ") && l.contains("mcp:fixture"));
    assert!(wait_until(Duration::from_secs(5), || stopped(&hosted.itse(&["status"]).1)));
    assert_eq!(mcp.tool_names().len(), 5, "the cached list");
    assert_eq!(hosted.starts(), 1, "a listing starts no stopped server");
}

#[test]
fn a_hosted_servers_ping_is_answered_and_its_other_requests_refused() {
    let hosted = Hosted::new(&[("ITSE_FIXTURE_MODE", "ask")], false);
    let (code, text) = hosted.itse(&["ext", "tools", "fixture"]);
    assert_eq!(code, Some(0), "{text}");
    let answer = |id: &str| hosted.messages().into_iter().find(|m| m["id"] == id && m["method"].is_null());
    assert!(wait_until(Duration::from_secs(2), || answer("r").is_some()));
    assert_eq!(answer("p").unwrap()["result"], json!({}));
    for id in ["s", "e", "r"] {
        assert_eq!(answer(id).unwrap()["error"]["code"], -32601, "{id}");
    }
}

#[test]
fn an_idle_hosted_server_is_stopped_and_its_list_stays_cached() {
    let hosted = Hosted::new(&[], false);
    hosted.settings(json!({"expose": true}));
    hosted.itse(&["daemon", "stop"]);
    let idle = |args: &[&str]| stdout(&hosted.env.command(&hosted.env.work(), args).env("ITSE_SERVER_IDLE_S", "1").output().unwrap());
    let text = idle(&["ext", "tools", "fixture"]);
    assert!(text.starts_with("OK ext tools fixture — 2 tools; exposed\n"), "{text}");
    let stopped = |s: &str| s.lines().any(|l| l.starts_with("stopped ") && l.contains("mcp:fixture"));
    assert!(wait_until(Duration::from_secs(5), || stopped(&idle(&["status"]))), "{}", idle(&["status"]));
    let status = idle(&["status"]);
    assert!(status.contains("\nmcp tools: 5 (") && stopped(&status), "listed from the cache, the server still stopped: {status}");
    assert_eq!(hosted.starts(), 1);
}

#[test]
fn a_caller_whose_path_lacks_a_servers_binary_stops_no_server_another_session_uses() {
    let env = Env::new();
    let (log, bin) = (env.dir.join("fixture.log"), env.dir.join("bin"));
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(FIXTURE, bin.join("itse-fixture")).unwrap();
    let server = json!({"id": "fixture", "protocol": "mcp", "transport": "stdio", "command": ["${pkg:itse-fixture}/itse-fixture", "mcp"], "env": {"ITSE_FIXTURE_LOG": log}});
    let packages = json!({"itse-fixture": {"purl": "pkg:generic/x/itse-fixture", "install": "path"}});
    let ext = env.dir.join("fix.json");
    fs::write(&ext, json!({"itse_ext": 1, "id": "fix", "packages": packages, "servers": [server]}).to_string()).unwrap();
    let hosted = Hosted { env, log };
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let narrow = |args: &[&str]| stdout(&hosted.env.command(&hosted.env.work(), args).env("PATH", "/usr/bin:/bin").env("ITSE_SERVER_IDLE_S", "60").output().unwrap());
    for args in [&["ext", "add", ext.to_str().unwrap()][..], &["use", "fix"]] {
        narrow(args);
    }
    hosted.settings(json!({"expose": true}));
    let mut mcp = hosted.env.mcp(&[("ITSE_SERVER_IDLE_S", "60"), ("PATH", &path)]);
    mcp.handshake("e2e");
    assert_eq!(mcp.tool_names(), [&CORE[..], &["fixture__echo", "fixture__slow"]].concat(), "started from the session's PATH");
    let status = narrow(&["status"]);
    assert!(status.contains("itse-fixture") && status.contains("not on PATH"), "the terminal can't start it: {status}");
    assert_eq!(mcp.call("fixture__echo", json!({"text": "still up"})), (false, "still up".into()));
    assert_eq!(hosted.starts(), 1, "the terminal's status stopped no server the session uses");
    narrow(&["unuse", "fix"]);
    narrow(&["status"]);
    assert_eq!(hosted.messages().last(), Some(&json!({"eof": true})), "an unused extension's server stops at the next resolve");
}
