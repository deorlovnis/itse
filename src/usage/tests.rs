use std::ffi::OsString;
use std::thread;
use std::time::Instant;

use serde_json::json;

use super::*;
use crate::sys::tests::TempDir;
use crate::verbs;

fn dirs(tmp: &TempDir) -> Dirs {
    let home: OsString = tmp.0.join("home").into();
    Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap()
}

fn call(verb: &str, outcome: &'static str) -> Event {
    Event::Call(Call {
        via: "cli",
        client: "cli".into(),
        client_v: None,
        verb: verb.into(),
        ext: None,
        ms: 3,
        outcome,
        err: None,
        bytes: 400,
        full: 400,
        trunc: false,
        warm: None,
        settled: None,
    })
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn records_have_exactly_their_keys() {
    let init = Record {
        v: 1,
        ts: "2026-11-04T18:31:07.412Z".into(),
        ws: "3f9a1c7e20b4".into(),
        sess: "9f3a1c7e5d2b4410".into(),
        ev: Event::Init { client: "claude-code".into(), client_v: Some("2.1.312".into()), proto: "2025-11-25".into(), tools: 4, list_bytes: 2306 },
    };
    let expected = json!({"v":1,"ev":"init","ts":"2026-11-04T18:31:07.412Z","ws":"3f9a1c7e20b4","sess":"9f3a1c7e5d2b4410","client":"claude-code","client_v":"2.1.312","proto":"2025-11-25","tools":4,"list_bytes":2306});
    assert_eq!(serde_json::to_value(&init).unwrap(), expected);
    let line: Value = serde_json::to_value(Record { v: 1, ts: "t".into(), ws: "w".into(), sess: "s".into(), ev: call("test", "fail") }).unwrap();
    let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
    let allowed = ["v", "ev", "ts", "ws", "sess", "via", "client", "client_v", "verb", "ext", "ms", "outcome", "err", "bytes", "full", "trunc", "warm", "settled"];
    assert!(keys.iter().all(|k| allowed.contains(k)) && keys.len() == allowed.len(), "{keys:?}");
    assert_eq!(line["ev"], "call");
}

#[test]
fn the_ws_id_is_salted() {
    let (a, b) = (TempDir::new("itse-usage"), TempDir::new("itse-usage"));
    let root = Path::new("/work/crm");
    let (ua, ub) = (Usage::new(&dirs(&a), root, false), Usage::new(&dirs(&b), root, false));
    ua.record(&BTreeMap::new(), "s", call("status", "ok"));
    ub.record(&BTreeMap::new(), "s", call("status", "ok"));
    let (wa, wb) = (lines(&ua.path())[0]["ws"].clone(), lines(&ub.path())[0]["ws"].clone());
    let salt = fs::read(dirs(&a).state.join("salt")).unwrap();
    assert_eq!(wa, json!(sys::sha256_hex(&[&salt[..], b"/work/crm"].concat())[..12]));
    assert_ne!(wa, json!(sys::sha256_hex(b"/work/crm")[..12]), "an unsalted hash of a path can be reversed by guessing");
    assert_ne!(wa, wb, "another machine's salt gives another id");
}

#[test]
fn sessions_are_hashed_and_clients_clipped() {
    let tmp = TempDir::new("itse-usage");
    let usage = Usage::new(&dirs(&tmp), &tmp.0, false);
    let env: BTreeMap<String, String> = [("ITSE_SESSION", "secret-session-zqx"), ("ITSE_CLIENT", "evil\x1b[31mclient/1.\x072")].map(|(k, v)| (k.into(), v.into())).into();
    let id = usage.ident("cli", &env);
    assert_eq!((id.session.len(), id.session.clone()), (16, id.sess.clone()), "a named session is one hash for both");
    assert!(id.sess.bytes().all(|b| b.is_ascii_hexdigit()) && !id.sess.contains("zqx"));
    assert_eq!((id.client.as_str(), id.client_v.as_deref()), ("evil?[31mclient", Some("1.?2")));
    let plain = usage.ident("cli", &BTreeMap::new());
    assert_eq!((plain.session.as_str(), plain.client.as_str(), plain.sess.len()), ("cli", "cli", 16), "a plain shell shares `cli`");
    assert_ne!(plain.sess, usage.ident("cli", &BTreeMap::new()).sess, "but each of its calls is its own in the log");
    let mcp = usage.ident("mcp", &BTreeMap::new());
    assert_eq!(mcp.session, mcp.sess, "an MCP connection is one session");
    assert_eq!(clip(&"x".repeat(100)).len(), 64);
}

#[test]
fn a_harness_identity_names_the_session_and_client() {
    let tmp = TempDir::new("itse-usage");
    let (usage, exts) = (Usage::new(&dirs(&tmp), &tmp.0, false), crate::manifest::extensions(&dirs(&tmp)).unwrap().0);
    let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    let harness = env(&[("CLAUDE_CODE_SESSION_ID", "s-1"), ("AI_AGENT", "claude-code_2-1-312_agent")]);
    let (mcp, hook) = (usage.ident("mcp", &Usage::identified(&exts, &harness)), usage.ident("hook", &env(&[("ITSE_SESSION", "s-1")])));
    assert_eq!((&mcp.sess, mcp.client.as_str(), mcp.client_v.as_deref()), (&hook.sess, "claude-code", Some("2.1.312")), "its MCP and hook calls are one session");
    let set = Usage::identified(&exts, &env(&[("CLAUDE_CODE_SESSION_ID", "s-1"), ("ITSE_SESSION", "s-2"), ("AI_AGENT", "other")]));
    assert_eq!((set["ITSE_SESSION"].as_str(), set.get("ITSE_CLIENT")), ("s-2", None), "what the caller set wins; an unmatched client is none");
}

#[test]
fn config_off_writes_nothing() {
    let tmp = TempDir::new("itse-usage");
    let d = dirs(&tmp);
    let usage = Usage::new(&d, &tmp.0, false);
    fs::create_dir_all(&d.config).unwrap();
    fs::write(d.config.join("config.json"), r#"{"usage": false}"#).unwrap();
    usage.record(&BTreeMap::new(), "s", call("status", "ok"));
    assert!(!usage.path().exists());
    let off: BTreeMap<String, String> = [("ITSE_USAGE".to_string(), "off".to_string())].into();
    fs::write(d.config.join("config.json"), r#"{"usage": true, "daemon_idle_minutes": 5}"#).unwrap();
    // The config is re-read when its mtime changes; make sure it does on a coarse clock.
    let later = SystemTime::now() + Duration::from_secs(2);
    File::options().write(true).open(d.config.join("config.json")).unwrap().set_modified(later).unwrap();
    usage.record(&off, "s", call("status", "ok"));
    assert!(!usage.path().exists(), "ITSE_USAGE=off in the caller's env");
    usage.record(&BTreeMap::new(), "s", call("status", "ok"));
    assert_eq!(lines(&usage.path()).len(), 1, "on again");
}

#[test]
fn an_unwritable_state_dir_still_answers() {
    let tmp = TempDir::new("itse-usage");
    let home: OsString = tmp.0.join("home").into();
    fs::write(tmp.0.join("state"), "a file where the state dir would be").unwrap();
    let state: OsString = tmp.0.join("state").into();
    let d = Dirs::from_env(|k| match k {
        "HOME" => Some(home.clone()),
        "XDG_STATE_HOME" => Some(state.clone()),
        _ => None,
    })
    .unwrap();
    let usage = Usage::new(&d, &tmp.0, false);
    usage.record(&BTreeMap::new(), "s", call("status", "ok"));
    assert!(usage.ident("cli", &[("ITSE_SESSION".to_string(), "x".to_string())].into()).sess.len() == 16, "an id without a salt is random");
}

#[test]
fn a_33_mb_log_rotates_once_under_two_writers() {
    let tmp = TempDir::new("itse-usage");
    let d = dirs(&tmp);
    let first = Usage::new(&d, &tmp.0, false);
    first.record(&BTreeMap::new(), "s", call("status", "ok"));
    File::options().write(true).open(first.path()).unwrap().set_len(ROTATE_AT + 1).unwrap();
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let (d, root) = (dirs(&tmp), tmp.0.clone());
            thread::spawn(move || {
                let usage = Usage::new(&d, &root, false);
                (0..50).for_each(|_| usage.record(&BTreeMap::new(), "s", call("status", "ok")));
            })
        })
        .collect();
    writers.into_iter().for_each(|w| w.join().unwrap());
    assert_eq!(fs::metadata(d.state.join("usage.1.jsonl")).unwrap().len(), ROTATE_AT + 1, "the big log was renamed once, not again");
    assert_eq!(lines(&first.path()).len(), 100, "no line went into the renamed file or got lost");
}

#[test]
fn append_costs_under_a_millisecond() {
    let tmp = TempDir::new("itse-usage");
    let usage = Usage::new(&dirs(&tmp), &tmp.0, false);
    usage.record(&BTreeMap::new(), "s", call("status", "ok"));
    let started = Instant::now();
    for _ in 0..2000 {
        usage.record(&BTreeMap::new(), "s", call("status", "ok"));
    }
    let mean = started.elapsed() / 2000;
    eprintln!("usage append: mean {} µs", mean.as_micros());
    assert!(mean < Duration::from_millis(1), "mean {mean:?}");
}

#[test]
fn stats_groups_by_verb_and_client() {
    let tmp = TempDir::new("itse-usage");
    let d = dirs(&tmp);
    fs::create_dir_all(&d.state).unwrap();
    let ts = sys::utc(SystemTime::now());
    let rec = |sess: &str, verb: &str, ext: Value, client: &str, ms: u64, outcome: &str, err: Value| {
        json!({"v":1,"ev":"call","ts":ts,"ws":"aaaaaaaaaaaa","sess":sess,"via":"mcp","client":client,"client_v":"2.1.312","verb":verb,"ext":ext,"ms":ms,"outcome":outcome,
            "err":err,"bytes":800,"full":2000800,"trunc":true,"warm":true,"settled":null})
    };
    let mut log =
        vec![json!({"v":1,"ev":"init","ts":ts,"ws":"aaaaaaaaaaaa","sess":"idle1","client":"claude-code","client_v":"2.1.312","proto":"2025-11-25","tools":3,"list_bytes":2000})];
    log.extend((1..=9).map(|i| rec("s1", "test", json!("gradle"), "claude-code", i * 100, "ok", Value::Null)));
    log.push(rec("s1", "test", json!("gradle"), "claude-code", 5000, "error", json!("server_crash")));
    log.push(rec("s2", "status", Value::Null, "cli", 7, "ok", Value::Null));
    log.push(json!({"v":1,"ev":"call","ts":"2020-01-01T00:00:00.000Z","ws":"b","sess":"old","verb":"find","outcome":"ok","ms":1}));
    fs::write(d.state.join("usage.jsonl"), log.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    let stats = |tokens: &[&str]| super::stats(&d, &verbs::parse_args(verbs::find("stats").unwrap(), tokens.iter().map(OsString::from)).unwrap(), &["test", "status", "outline"]);
    let text = stats(&[]).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("usage ") && lines[0].contains(" (7d): 11 calls, 3 sessions, 1 workspaces; log "), "{text}");
    assert!(lines[1].starts_with("verb            calls  ok   fail err   p50ms  p95ms"), "{text}");
    assert_eq!(lines[2], "status              1  100% -    -         7      7        200    2.0 MB   100%  100%");
    assert_eq!(lines[3], "test (gradle)      10  90%  -    10%     500  5,000        200   20.0 MB   100%  100%");
    assert_eq!(lines[4], "connected, never called: 1 of 3 sessions (claude-code 2.1.312)");
    assert_eq!(lines[5], "offered, never called: outline");
    assert_eq!(lines[6], "errors: server_crash 1 (gradle)");
    let by_client = stats(&["--by", "client", "--since", "2019-12-31"]).unwrap();
    assert!(by_client.contains("\nclaude-code 2.1.312    10  90% "), "{by_client}");
    assert!(by_client.contains("\ncli 2.1.312             1 "), "{by_client}");
    let v: Value = serde_json::from_str(&stats(&["--json"]).unwrap()).unwrap();
    assert_eq!((v["calls"].clone(), v["idle_sessions"].clone(), v["never_called"].clone()), (json!(11), json!(1), json!(["outline"])));
    assert_eq!(v["groups"][1]["p95_ms"], 5000);
    assert_eq!(stats(&["--since", "yesterday"]).unwrap_err().to_string(), "--since takes 7d, 24h or YYYY-MM-DD, not 'yesterday'");
    assert!(stats(&["--by", "colour"]).is_err());
    assert!(stats(&["--since", "200000000000000d"]).unwrap().starts_with("usage "), "a span past the epoch counts from it");
}
