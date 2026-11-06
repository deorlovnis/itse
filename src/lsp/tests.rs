use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;

use super::*;
use crate::rpc::{self, MAX_FRAME};
use crate::sys::tests::TempDir;

/// git for a test's own temp repo, with its HOME and identity and no system config. A git hook
/// that runs the tests exports GIT_DIR and friends (an absolute GIT_DIR from a worktree), which
/// would point every call at the caller's repo, so no `GIT_*` var of ours reaches it.
fn test_git(dir: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
        cmd.env_remove(key);
    }
    let ident = [("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"), ("GIT_CONFIG_NOSYSTEM", "1")];
    cmd.args(args).current_dir(dir).env("HOME", home).envs(ident);
    cmd
}

/// The server's side of an in-process pair: every message the client sent, and its writer, so a
/// test can push notifications and requests as a server would. It answers requests with `answer`.
struct Fake {
    seen: Arc<Mutex<Vec<Value>>>,
    out: Arc<Mutex<UnixStream>>,
}

impl Fake {
    fn start(answer: impl Fn(&str, &Value) -> Value + Send + 'static) -> (Self, Arc<Peer>, Arc<Slot>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let slot = Arc::new(Slot::default());
        let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch: 0, settings: settings(), folders: json!([{"uri": "file:///w", "name": "w"}]), log: None };
        let peer = Peer::start(BufReader::new(ours.try_clone().unwrap()), ours, Framing::Headers, Dialect::Lsp, Arc::new(reader));
        let (seen, out) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(theirs.try_clone().unwrap())));
        let (log, writer) = (seen.clone(), out.clone());
        thread::spawn(move || {
            let mut r = BufReader::new(theirs);
            while let Ok(Some(msg)) = rpc::read(&mut r, Framing::Headers, MAX_FRAME) {
                if let (Some(method), false) = (msg["method"].as_str(), msg["id"].is_null()) {
                    let reply = json!({"jsonrpc": "2.0", "id": msg["id"], "result": answer(method, &msg["params"])});
                    rpc::write(&mut *lock(&writer), Framing::Headers, &reply).unwrap();
                }
                lock(&log).push(msg);
            }
        });
        (Self { seen, out }, peer, slot)
    }

    fn send(&self, msg: Value) {
        rpc::write(&mut *lock(&self.out), Framing::Headers, &msg).unwrap();
    }

    /// The client's messages once there are `n`, waiting up to 2 s.
    fn wait(&self, n: usize) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(2);
        while lock(&self.seen).len() < n && Instant::now() < end {
            thread::sleep(Duration::from_millis(5));
        }
        lock(&self.seen).clone()
    }

    /// The methods and `textDocument` of what the client sent, from message `from` on.
    fn docs(&self, from: usize) -> Vec<(String, Value)> {
        thread::sleep(Duration::from_millis(100));
        lock(&self.seen)[from..].iter().map(|m| (m["method"].as_str().unwrap_or("").to_string(), m["params"]["textDocument"].clone())).collect()
    }
}

fn settings() -> Value {
    json!({"fixture": {"a": {"b": 1}}, "rust-analyzer": {"cargo": {"targetDir": "/cache/ra"}}})
}

fn ask(peer: Arc<Peer>, slot: Arc<Slot>, root: &Path, git: bool) -> Ask {
    let launch = Launch { id: "fixture".into(), filetypes: vec!["rs".into()], languages: [("rs".into(), "rust".into())].into(), ..Launch::default() };
    Ask { handle: "lsp:fixture".into(), peer, slot, root: root.into(), git, launch, notes: vec![], _held: None, fresh: false, epoch: 0 }
}

fn notify(slot: &Slot, method: &str, params: Value) {
    let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch: 0, settings: settings(), folders: json!([]), log: None };
    reader.notify(method, params);
}

#[test]
fn initialize_sends_the_exact_capabilities_then_initialized_and_the_settings() {
    for (encoding, utf16) in [(json!("utf-8"), false), (json!("utf-16"), true), (Value::Null, true)] {
        let (fake, peer, slot) = Fake::start(move |_, _| json!({"capabilities": {"positionEncoding": encoding}}));
        let launch = Launch { init_options: json!({"checkOnSave": true}), settings: settings(), ..Launch::default() };
        initialize(&peer, &slot, &launch, Path::new("/w s"), json!([{"uri": "file:///w%20s", "name": "w s"}])).unwrap();
        let seen = fake.wait(3);
        let kinds: Vec<u32> = (1..=26).collect();
        let want = json!({"general":{"positionEncodings":["utf-8","utf-16"]},
         "window":{"workDoneProgress":true,"showMessage":{}},
         "workspace":{"configuration":true,"workspaceFolders":true,"didChangeWatchedFiles":{"dynamicRegistration":false},
                      "diagnostics":{"refreshSupport":true},"executeCommand":{},"symbol":{"symbolKind":{"valueSet":kinds}}},
         "textDocument":{"synchronization":{"didSave":true},"publishDiagnostics":{"relatedInformation":true,"versionSupport":true},
           "documentSymbol":{"hierarchicalDocumentSymbolSupport":true,"symbolKind":{"valueSet":kinds}},
           "hover":{"contentFormat":["plaintext","markdown"]},"definition":{"linkSupport":false},"references":{}},
         "experimental":{"serverStatusNotification":true}});
        let p = &seen[0]["params"];
        assert_eq!((seen[0]["method"].as_str(), &p["capabilities"]), (Some("initialize"), &want));
        assert_eq!((p["processId"].as_u64(), p["rootUri"].as_str()), (Some(process::id() as u64), Some("file:///w%20s")));
        assert_eq!(p["workspaceFolders"], json!([{"uri": "file:///w%20s", "name": "w s"}]));
        assert_eq!(p["clientInfo"], json!({"name": "itse", "version": env!("CARGO_PKG_VERSION")}));
        assert_eq!(p["initializationOptions"], json!({"checkOnSave": true}));
        assert_eq!((seen[1]["method"].as_str(), seen[2]["method"].as_str()), (Some("initialized"), Some("workspace/didChangeConfiguration")));
        assert_eq!(seen[2]["params"], json!({"settings": settings()}));
        assert_eq!(lock(&slot.live).lsp.utf16, utf16, "{seen:?}");
    }
}

#[test]
fn a_server_that_ends_once_initialize_answered_has_started_and_the_next_request_is_a_crash() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let slot = Arc::new(Slot::default());
    let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch: 0, settings: settings(), folders: json!([]), log: None };
    let peer = Peer::start(BufReader::new(ours.try_clone().unwrap()), ours, Framing::Headers, Dialect::Lsp, Arc::new(reader));
    thread::spawn(move || {
        let init = rpc::read(&mut BufReader::new(&theirs), Framing::Headers, MAX_FRAME).unwrap().unwrap();
        theirs.shutdown(std::net::Shutdown::Read).unwrap();
        // The client's answer to this fails to write, which ends its writer before initialize returns.
        rpc::write(&mut &theirs, Framing::Headers, &json!({"jsonrpc": "2.0", "id": "p", "method": "window/workDoneProgress/create", "params": {"token": "t"}})).unwrap();
        thread::sleep(Duration::from_millis(200));
        rpc::write(&mut &theirs, Framing::Headers, &json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}})).unwrap();
    });
    initialize(&peer, &slot, &Launch::default(), Path::new("/w"), json!([])).expect("answered, so started");
    assert!(peer.notify("initialized", json!({})).is_err(), "the notifications could not be written");
    let tmp = TempDir::new("itse-lsp");
    let ask = ask(peer.clone(), slot, &tmp.0, false);
    let end = Instant::now() + Duration::from_secs(2);
    while peer.closed().is_none() && Instant::now() < end {
        thread::sleep(Duration::from_millis(5));
    }
    let e = ask.request("workspace/symbol", json!({"query": "x"}), &Cancel::default()).unwrap_err();
    assert_eq!(e.to_string(), "server_crash: lsp:fixture ended during the call (eof); the next call starts it again");
}

#[test]
fn server_requests_are_answered() {
    let (fake, _peer, slot) = Fake::start(|_, _| Value::Null);
    let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch: 0, settings: settings(), folders: json!([{"uri": "file:///w", "name": "w"}]), log: None };
    let config = json!({"items": [{"section": "fixture.a"}, {"section": "fixture.a.b"}, {"section": "rust-analyzer"}, {"section": "nope.x"}, {}]});
    assert_eq!(reader.request("workspace/configuration", config), Ok(json!([{"b": 1}, 1, {"cargo": {"targetDir": "/cache/ra"}}, null, settings()])));
    assert_eq!(reader.request("window/workDoneProgress/create", json!({"token": "t1"})), Ok(Value::Null));
    assert_eq!(lock(&slot.live).lsp.progress.get("\"t1\""), Some(&None), "a created token is tracked, not yet running");
    let reg = json!({"registrations": [{"id": "1", "method": "workspace/didChangeWatchedFiles"}, {"id": "2", "method": "textDocument/formatting"}]});
    assert_eq!(reader.request("client/registerCapability", reg), Ok(Value::Null));
    assert_eq!(reader.request("client/unregisterCapability", json!({"unregisterations": [{"id": "2", "method": "textDocument/formatting"}]})), Ok(Value::Null));
    assert_eq!(lock(&slot.live).lsp.registered, BTreeSet::from(["workspace/didChangeWatchedFiles".to_string()]));
    for refresh in ["workspace/diagnostic/refresh", "workspace/semanticTokens/refresh", "workspace/inlayHint/refresh", "workspace/codeLens/refresh"] {
        assert_eq!(reader.request(refresh, Value::Null), Ok(Value::Null), "{refresh}");
    }
    assert_eq!(reader.request("workspace/workspaceFolders", Value::Null), Ok(json!([{"uri": "file:///w", "name": "w"}])));
    assert_eq!(reader.request("window/showMessageRequest", json!({"type": 3, "message": "reload?", "actions": [{"title": "yes"}]})), Ok(Value::Null));
    assert!(lock(&slot.live).last.back().is_some_and(|l| l.ends_with(" window/showMessageRequest: reload?")), "{:?}", lock(&slot.live).last);
    assert_eq!(reader.request("window/showDocument", json!({"uri": "file:///w/a.rs"})), Ok(json!({"success": false})));
    assert_eq!(reader.request("workspace/applyEdit", json!({"edit": {}})), Ok(json!({"applied": false, "failureReason": "itse applies no edits"})));
    assert_eq!(reader.request("workspace/executeClientCommand", Value::Null), Err((-32601, "method not found: workspace/executeClientCommand".into())));
    // Through the peer: the reader thread answers while nobody waits on it.
    fake.send(json!({"jsonrpc": "2.0", "id": 7, "method": "workspace/configuration", "params": {"items": [{"section": "fixture.a.b"}]}}));
    fake.send(json!({"jsonrpc": "2.0", "id": 8, "method": "no/such"}));
    let seen = fake.wait(2);
    assert_eq!(seen[0], json!({"jsonrpc": "2.0", "id": 7, "result": [1]}));
    assert_eq!(seen[1]["error"]["code"], -32601);
}

#[test]
fn progress_makes_the_index_cold_then_busy_then_warm() {
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let mut ask = ask(peer, slot.clone(), &tmp.0, false);
    let progress = |kind: &str, extra: Value| {
        let mut value = json!({"kind": kind});
        value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        notify(&slot, "$/progress", json!({"token": "idx", "value": value}));
    };
    assert_eq!(ask.index(), "cold");
    progress("begin", json!({"title": "Indexing", "message": "1/3", "percentage": 33}));
    assert_eq!(ask.index(), "cold (Indexing 1/3 (33%))", "the first settle hasn't happened");
    assert!(!ask.settle(Instant::now() + Duration::from_millis(200), &Cancel::default()), "progress runs");
    progress("report", json!({"message": "2/3", "percentage": 66}));
    assert_eq!(ask.index(), "cold (Indexing 2/3 (66%))", "a report keeps the title");
    progress("end", json!({}));
    assert!(ask.settle(Instant::now() + Duration::from_secs(2), &Cancel::default()));
    assert_eq!(ask.index(), "warm");
    progress("begin", json!({"title": "Indexing"}));
    assert_eq!(ask.index(), "busy (Indexing)");
    progress("end", json!({}));
    notify(&slot, "experimental/serverStatus", json!({"health": "ok", "quiescent": false}));
    assert_eq!(ask.index(), "busy");
    assert!(!ask.settle(Instant::now() + Duration::from_millis(400), &Cancel::default()), "not quiescent");
    notify(&slot, "experimental/serverStatus", json!({"health": "ok", "quiescent": true}));
    assert_eq!(ask.index(), "warm");
    // A reader left from an earlier start records nothing.
    (lock(&slot.live).epoch, ask.epoch) = (1, 1);
    progress("begin", json!({"title": "stale"}));
    assert_eq!(ask.index(), "warm");
}

#[test]
fn settle_times_out_to_settled_false() {
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let ask = ask(peer, slot.clone(), &tmp.0, false);
    notify(&slot, "$/progress", json!({"token": 1, "value": {"kind": "begin", "title": "Indexing"}}));
    let started = Instant::now();
    assert!(!ask.settle(started + Duration::from_millis(300), &Cancel::default()));
    assert!((300..600).contains(&started.elapsed().as_millis()), "{:?}", started.elapsed());
    assert_eq!(ask.index(), "cold (Indexing)", "an unsettled index stays cold");
    let (cancel, started) = (Cancel::default(), Instant::now());
    cancel.cancel();
    assert!(!ask.settle(started + Duration::from_secs(5), &cancel) && started.elapsed() < Duration::from_secs(1), "a cancelled call stops waiting");
    let mut stale = self::ask(ask.peer.clone(), slot.clone(), &tmp.0, false);
    stale.epoch = 1;
    fs::write(tmp.0.join("b.rs"), "fn b() {}\n").unwrap();
    let refused = stale.open(&tmp.0.join("b.rs")).unwrap_err().to_string();
    assert!(refused.starts_with("server_crash: lsp:fixture started again during the call"), "a query of an earlier start changes no documents: {refused}");
    notify(&slot, "$/progress", json!({"token": 1, "value": {"kind": "end"}}));
    // A file told about waits for its push, 1.5 s at most, whichever call told it; a push for it ends the wait.
    let file = tmp.0.join("a.rs");
    lock(&slot.live).lsp.pending.insert(file.clone(), Instant::now());
    let started = Instant::now();
    assert!(ask.settle(started + Duration::from_secs(5), &Cancel::default()));
    assert!(started.elapsed() >= PUSH - Duration::from_millis(50), "{:?}", started.elapsed());
    lock(&slot.live).lsp.pending.insert(file.clone(), Instant::now());
    let slot2 = slot.clone();
    let push = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        notify(&slot2, "textDocument/publishDiagnostics", json!({"uri": uri(&file), "diagnostics": []}));
    });
    let started = Instant::now();
    assert!(ask.settle(started + Duration::from_secs(5), &Cancel::default()), "another call's settle waits for it too");
    assert!(started.elapsed() < Duration::from_millis(1200), "{:?}", started.elapsed());
    push.join().unwrap();
    let mut gone = self::ask(ask.peer.clone(), slot.clone(), &tmp.0, false);
    gone.epoch = 7;
    assert!(!gone.settle(Instant::now() + Duration::from_secs(1), &Cancel::default()) && gone.index() == "cold", "a call of an earlier start never settles the new one");
}

#[test]
fn a_save_that_starts_no_check_is_sent_once_more_then_unsettled() {
    let (fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let mut ask = ask(peer, slot.clone(), &tmp.0, false);
    let a = tmp.0.join("a.rs");
    fs::write(&a, "fn a() {}\n").unwrap();
    ask.open(&a).unwrap();
    let check = |slot: &Slot| {
        for k in ["begin", "end"] {
            notify(slot, "$/progress", json!({"token": "check", "value": {"kind": k, "title": "cargo check"}}));
        }
    };
    let saves = |fake: &Fake| fake.docs(0).iter().filter(|d| d.0 == "textDocument/didSave").count();
    let edit = |n: u32| fs::write(&a, format!("fn a() {{ {n} }}\n")).unwrap();
    edit(1);
    ask.sync();
    check(&slot);
    assert!(ask.settle(Instant::now() + Duration::from_secs(5), &Cancel::default()));
    assert!(lock(&slot.live).lsp.checks, "a check right after a save: the server checks on save");
    edit(2);
    ask.sync();
    let (slot2, started) = (slot.clone(), Instant::now());
    let late = thread::spawn(move || {
        thread::sleep(PUSH + Duration::from_millis(200));
        check(&slot2);
    });
    assert!(ask.settle(started + Duration::from_secs(5), &Cancel::default()), "the save sent again started one");
    assert!(started.elapsed() >= PUSH && saves(&fake) == 3, "{:?}, {} saves", started.elapsed(), saves(&fake));
    late.join().unwrap();
    edit(3);
    ask.sync();
    let started = Instant::now();
    assert!(!ask.settle(started + Duration::from_secs(5), &Cancel::default()), "no check after either save");
    assert!(started.elapsed() < Duration::from_secs(4) && saves(&fake) == 5, "{:?}", started.elapsed());
    assert!(!ask.settle(Instant::now() + Duration::from_secs(5), &Cancel::default()), "and none until a check or the next save");
    check(&slot);
    assert!(ask.settle(Instant::now() + Duration::from_secs(5), &Cancel::default()));
}

#[test]
fn diagnostics_past_2000_are_counted_not_kept() {
    let slot = Slot::default();
    let diags = |n: usize, message: &str| {
        (0..n).map(|i| json!({"range": {"start": {"line": i, "character": 2}, "end": {"line": i, "character": 5}}, "message": message})).collect::<Vec<_>>()
    };
    notify(&slot, "textDocument/publishDiagnostics", json!({"uri": "file:///w/a.rs", "version": 3, "diagnostics": diags(1500, "a")}));
    notify(&slot, "textDocument/publishDiagnostics", json!({"uri": "file:///w/b.rs", "diagnostics": diags(1000, "b")}));
    let live = lock(&slot.live);
    let (a, b) = (&live.lsp.diags["file:///w/a.rs"], &live.lsp.diags["file:///w/b.rs"]);
    assert_eq!((live.lsp.kept, a.items.len(), a.dropped, b.items.len(), b.dropped), (2000, 1500, 0, 500, 500));
    assert_eq!((a.seq, b.seq), (1, 2));
    assert_eq!(a.items[7], Diag { range: [7, 2, 7, 5], severity: 1, code: None, source: None, message: "a".into() });
    drop(live);
    let long = "é".repeat(400);
    let one = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}, "severity": 2, "code": 308, "source": "rustc", "message": long}]);
    notify(&slot, "textDocument/publishDiagnostics", json!({"uri": "file:///w/a.rs", "diagnostics": one}));
    let live = lock(&slot.live);
    assert_eq!(live.lsp.kept, 501, "a new push replaces the old one's items");
    let d = &live.lsp.diags["file:///w/a.rs"].items[0];
    assert_eq!((d.severity, d.code.as_deref(), d.source.as_deref(), d.message.len()), (2, Some("308"), Some("rustc"), 512), "messages are clipped on a char boundary");
}

#[test]
fn an_on_disk_edit_is_didchange_and_didsave_and_a_delete_didclose() {
    let (fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let mut ask = ask(peer, slot, &tmp.0, false);
    let a = tmp.0.join("a.rs");
    fs::write(&a, "fn a() {}\n").unwrap();
    let uri = ask.open(&a).unwrap();
    assert_eq!(fake.docs(0), [("textDocument/didOpen".into(), json!({"uri": uri, "languageId": "rust", "version": 1, "text": "fn a() {}\n"}))]);
    assert_eq!(ask.sync(), Vec::<PathBuf>::new(), "nothing changed");
    assert_eq!(ask.notes, [NO_GIT]);
    fs::write(&a, "fn a() { FIXTURE_ERROR }\n").unwrap();
    assert_eq!(ask.sync(), std::slice::from_ref(&a));
    let seen = fake.wait(3);
    assert_eq!(seen[1]["params"], json!({"textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": "fn a() { FIXTURE_ERROR }\n"}]}));
    assert_eq!((seen[1]["method"].as_str(), seen[2]["method"].as_str()), (Some("textDocument/didChange"), Some("textDocument/didSave")));
    // A replace by rename keeps size and maybe mtime, but not the inode.
    let b = tmp.0.join("b.tmp");
    fs::write(&b, "fn a() { FIXTURE_ERROR }\n").unwrap();
    fs::rename(&b, &a).unwrap();
    assert_eq!(ask.sync(), std::slice::from_ref(&a));
    assert_eq!(fake.docs(3).iter().map(|d| (d.0.as_str(), d.1["version"].as_i64())).collect::<Vec<_>>(), [("textDocument/didChange", Some(3)), ("textDocument/didSave", None)]);
    fs::remove_file(&a).unwrap();
    assert_eq!(ask.sync(), std::slice::from_ref(&a));
    assert_eq!(fake.docs(5), [("textDocument/didClose".into(), json!({"uri": uri}))]);
    assert_eq!(ask.sync(), Vec::<PathBuf>::new(), "a closed document is forgotten");
}

#[test]
fn opened_documents_are_kept_50_at_most_and_big_files_refused() {
    let (fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let ask = ask(peer, slot, &tmp.0, false);
    let file = |n: usize| tmp.0.join(format!("f{n}.rs"));
    for n in 0..51 {
        fs::write(file(n), "x").unwrap();
        ask.open(&file(if n == 50 { 0 } else { n })).unwrap();
    }
    ask.open(&file(50)).unwrap();
    let docs = fake.docs(0);
    assert_eq!(docs.len(), 52, "50 opens, the first one again only moved to the end, then one more open and one close");
    assert_eq!(docs[51], ("textDocument/didClose".into(), json!({"uri": uri(&file(1))})), "the least recently used goes");
    let big = tmp.0.join("big.rs");
    File::create(&big).unwrap().set_len(MAX_FILE + 1).unwrap();
    let refused = ask.open(&big).unwrap_err();
    assert!(refused.to_string().starts_with("file_too_large: ") && err_class(&refused) == "file_too_large", "{refused}");
}

#[test]
fn unopened_files_are_opened_and_saved_from_a_lockless_git_status() {
    let tmp = TempDir::new("itse-lsp");
    let git = |args: &[&str]| {
        // The test's own git runs no fsmonitor either.
        let git = test_git(&tmp.0, &tmp.0, &["-c", "core.fsmonitor=false"]).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
        git.is_ok_and(|s| s.success())
    };
    fs::write(tmp.0.join("a.rs"), "fn a() {}\n").unwrap();
    fs::write(tmp.0.join("notes.md"), "x\n").unwrap();
    if !(git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-qm", "one"])) {
        return eprintln!("skipped: needs git");
    }
    let ran = tmp.0.join("fsmonitor-ran");
    assert!(git(&["config", "core.fsmonitor", &format!("touch '{}'; false #", ran.display())]));
    let index = || fs::metadata(tmp.0.join(".git/index")).map(|m| (m.ino(), m.mtime(), m.mtime_nsec())).unwrap();
    let (fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let mut ask = ask(peer, slot, &tmp.0, true);
    fs::write(tmp.0.join("a.rs"), "fn a() { 1 }\n").unwrap();
    let before = index();
    assert_eq!(ask.sync(), Vec::<PathBuf>::new(), "the first status after a start only notes what is changed");
    fs::write(tmp.0.join("b.rs"), "fn b() {}\n").unwrap();
    fs::write(tmp.0.join("notes.md"), "y\n").unwrap();
    assert_eq!(ask.sync(), [tmp.0.join("b.rs")], "a new untracked file of the server's filetypes, at once");
    let b = uri(&tmp.0.join("b.rs"));
    assert_eq!(
        fake.docs(0),
        [("textDocument/didOpen".into(), json!({"uri": b, "languageId": "rust", "version": 1, "text": "fn b() {}\n"})), ("textDocument/didSave".into(), json!({"uri": b}))]
    );
    assert_eq!(ask.sync(), Vec::<PathBuf>::new(), "told once");
    assert!(git(&["commit", "-qam", "two"]));
    let after_commit = index();
    assert_eq!(ask.sync(), [tmp.0.join("a.rs")], "HEAD moved: the files the commit changed");
    assert!(ask.notes.is_empty(), "{:?}", ask.notes);
    assert_eq!(index(), after_commit, "git status took no index.lock and rewrote no index");
    assert_ne!(before, after_commit, "the commit itself did");
    // A checkout that changes no file of its filetypes: the newest opened document is saved.
    assert!(git(&["checkout", "-q", "-b", "notes"]) && fs::write(tmp.0.join("notes.md"), "z\n").is_ok() && git(&["commit", "-qam", "notes"]));
    let from = fake.docs(0).len();
    assert_eq!(ask.sync(), [tmp.0.join("a.rs")]);
    assert_eq!(fake.docs(from), [("textDocument/didSave".into(), json!({"uri": uri(&tmp.0.join("a.rs"))}))], "one save, no change");
    assert!(git(&["checkout", "-q", "-"]));
    assert_eq!(ask.sync(), [tmp.0.join("a.rs")], "and back");
    // An unopened file that leaves the list because it was reverted changed on disk too.
    fs::write(tmp.0.join("notes.rs"), "fn n() {}\n").unwrap();
    assert!(git(&["add", "notes.rs"]) && git(&["commit", "-qm", "n"]));
    ask.sync();
    let (ours, from) = (tmp.0.join("notes.rs"), fake.docs(0).len());
    lock(&ask.slot.docs).open.retain(|d| d.path != ours);
    fs::write(&ours, "fn n() { 2 }\n").unwrap();
    assert_eq!(ask.sync(), std::slice::from_ref(&ours));
    lock(&ask.slot.docs).open.retain(|d| d.path != ours);
    assert!(git(&["checkout", "-q", "--", "notes.rs"]));
    assert_eq!(ask.sync(), std::slice::from_ref(&ours), "reverted");
    assert_eq!(fake.docs(from).iter().filter(|d| d.0 == "textDocument/didOpen").count(), 2);
    let after_checkout = index();
    // Past 50, the newest 50 by mtime.
    for n in 0..55 {
        let c = tmp.0.join(format!("c{n}.rs"));
        fs::write(&c, "fn c() {}\n").unwrap();
        File::options().write(true).open(&c).unwrap().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + n)).unwrap();
    }
    let mut told = ask.sync();
    told.sort();
    let mut newest: Vec<PathBuf> = (5..55).map(|n| tmp.0.join(format!("c{n}.rs"))).collect();
    newest.sort();
    assert_eq!(told, newest, "50 didSaves at most per sync, the newest");
    assert_eq!(index(), after_checkout, "the checkout was the test's own");
    assert!(!ran.exists(), "the repo's core.fsmonitor never ran");
}

#[test]
fn a_sync_runs_none_of_the_commands_the_repos_config_names() {
    let tmp = TempDir::new("itse-lsp");
    let (root, sub) = (tmp.0.clone(), tmp.0.join("sub"));
    let git = |dir: &Path, args: &[&str]| {
        let git = test_git(dir, &root, args).stdout(Stdio::null()).stderr(Stdio::null()).status();
        git.is_ok_and(|s| s.success())
    };
    let write = |path: &Path, text: &str| fs::create_dir_all(path.parent().unwrap()).and_then(|()| fs::write(path, text)).unwrap();
    write(&root.join(".gitattributes"), "a.rs filter=clean diff=tx\nb.rs filter=proc diff=tx\n");
    write(&root.join("a.rs"), "fn a() {}\n");
    write(&root.join("b.rs"), "fn b() {}\n");
    write(&sub.join(".gitattributes"), "*.rs filter=inner\n");
    write(&sub.join("s.rs"), "fn s() {}\n");
    // A nested repo, which the parent's status would check with the nested repo's own config.
    let made = git(&sub, &["init", "-q"]) && git(&sub, &["add", "-A"]) && git(&sub, &["commit", "-qm", "s"]);
    if !(made && git(&root, &["init", "-q"]) && git(&root, &["add", "-A"]) && git(&root, &["commit", "-qm", "one"])) {
        return eprintln!("skipped: needs git");
    }
    assert!(fs::write(root.join("a.rs"), "fn c() {}\n").is_ok() && git(&root, &["commit", "-qam", "two"]));
    // Planted once the test's own commits are made: every one touches the same marker. The filters
    // come from a file the repo's config includes, and the process one is required, so git fails
    // rather than skip it unless it is blanked; the nested repo has a filter of its own.
    let ran = root.join("ran");
    let touch = format!("touch '{}'; cat", ran.display());
    write(&root.join("drivers.cfg"), &format!("[filter \"clean\"]\n\tclean = {touch}\n[filter \"proc\"]\n\tprocess = {touch}\n\trequired = true\n"));
    let fsmonitor = format!("{touch} #");
    for (key, value) in [("include.path", "../drivers.cfg"), ("diff.tx.textconv", &touch), ("diff.external", &touch), ("core.fsmonitor", &fsmonitor)] {
        assert!(git(&root, &["config", key, value]));
    }
    assert!(git(&sub, &["config", "filter.inner.clean", &touch]));
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let mut ask = ask(peer, slot, &root, true);
    ask.sync();
    // Same sizes, new mtimes: git rehashes them, which is where a clean or process filter runs.
    let later = SystemTime::now() + Duration::from_secs(5);
    for (path, text) in [(root.join("a.rs"), "fn d() {}\n"), (root.join("b.rs"), "fn e() {}\n"), (sub.join("s.rs"), "fn t() {}\n")] {
        write(&path, text);
        File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
    }
    assert_eq!(ask.sync(), [root.join("b.rs"), root.join("a.rs")], "git status listed both");
    // HEAD moves back: the diff between the two commits, where a textconv or an external diff runs.
    assert!(git(&root, &["-c", "core.fsmonitor=false", "reset", "-q", "--soft", "HEAD~1"]));
    ask.sync();
    assert!(ask.notes.is_empty(), "{:?}", ask.notes);
    assert!(!ran.exists(), "no filter, textconv, external diff or fsmonitor of either repo ran");
    // The trace index's listing reads the index only.
    let listed = sys::git(&root).args(["ls-files", "-z"]).output().unwrap();
    assert!(listed.status.success() && listed.stdout.starts_with(b".gitattributes\0a.rs\0b.rs\0") && !ran.exists());
}

#[test]
fn a_sync_in_a_partial_clone_fetches_nothing_through_the_repos_remote() {
    let tmp = TempDir::new("itse-lsp");
    let (src, cl, ran) = (tmp.0.join("src"), tmp.0.join("cl"), tmp.0.join("ran"));
    let git = |dir: &Path, args: &[&str]| {
        let git = test_git(dir, &tmp.0, &["-c", "core.fsmonitor=false"]).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
        git.is_ok_and(|s| s.success())
    };
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(src.join("d/a.rs"), "fn a() {}\n").unwrap();
    let made = git(&src, &["init", "-q"]) && git(&src, &["add", "-A"]) && git(&src, &["commit", "-qm", "one"]) && fs::write(src.join("d/a.rs"), "fn b() {}\n").is_ok();
    let made = made && git(&src, &["commit", "-qam", "two"]) && git(&src, &["config", "uploadpack.allowFilter", "true"]);
    // Commits only: the trees of the first one stay on the remote, to be fetched when something reads them.
    if !(made && git(&tmp.0, &["clone", "-q", "--no-local", "--filter=tree:0", &format!("file://{}", src.display()), "cl"])) {
        return eprintln!("skipped: needs git with partial clones");
    }
    assert!(git(&cl, &["config", "remote.origin.uploadpack", &format!("touch '{}'; git-upload-pack", ran.display())]));
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let mut ask = ask(peer, slot, &cl, true);
    ask.sync();
    // HEAD moves to the first commit, so the sync's diff reads a tree the clone lacks.
    assert!(git(&cl, &["update-ref", "HEAD", "HEAD~1"]));
    ask.sync();
    assert!(!ran.exists(), "the repo's remote ran no upload-pack command");
    assert!(git(&cl, &["diff", "--name-only", "HEAD", "HEAD@{1}"]) && ran.exists(), "a plain git would have fetched through it");
}

#[test]
fn a_link_to_a_device_or_a_fifo_is_never_read_by_a_sync_or_an_open() {
    let tmp = TempDir::new("itse-lsp");
    let git = |args: &[&str]| {
        let git = test_git(&tmp.0, &tmp.0, &["-c", "core.fsmonitor=false"]).args(args).stderr(Stdio::null()).output();
        git.is_ok_and(|o| o.status.success())
    };
    fs::write(tmp.0.join("a.rs"), "fn a() {}\n").unwrap();
    if !(git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-qm", "one"])) {
        return eprintln!("skipped: needs git");
    }
    let (fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let mut ask = ask(peer, slot, &tmp.0, true);
    let (a, zero, fifo) = (tmp.0.join("a.rs"), tmp.0.join("zero.rs"), tmp.0.join("fifo.rs"));
    ask.open(&a).unwrap();
    ask.sync();
    // Untracked, then committed (as a pull would bring it): git lists it both ways.
    std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
    let at = Instant::now();
    assert_eq!(ask.sync(), Vec::<PathBuf>::new(), "untracked");
    assert!(git(&["add", "zero.rs"]) && git(&["commit", "-qm", "zero"]));
    assert_eq!(ask.sync(), std::slice::from_ref(&a), "HEAD moved: only the opened document's one save");
    // An opened document replaced by one is closed, as if deleted.
    fs::remove_file(&a).unwrap();
    std::os::unix::fs::symlink("/dev/zero", &a).unwrap();
    assert_eq!(ask.sync(), std::slice::from_ref(&a));
    assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
    let sent: Vec<String> = fake.docs(0).into_iter().map(|(m, d)| format!("{m} {}", d["uri"].as_str().unwrap_or(""))).collect();
    assert_eq!(sent, [format!("textDocument/didOpen {}", uri(&a)), format!("textDocument/didSave {}", uri(&a)), format!("textDocument/didClose {}", uri(&a))]);
    assert_eq!(ask.open(&zero).unwrap_err().to_string(), format!("no file {}", zero.display()));
    // A FIFO swapped in after the check opens at once and is refused, with no writer to wait for.
    if Command::new("mkfifo").arg(&fifo).status().is_ok_and(|s| s.success()) {
        assert_eq!(read(&fifo).unwrap_err().to_string(), format!("no file {}", fifo.display()));
    }
    assert!(read(&zero).is_err() && ask.notes.is_empty(), "{:?}", ask.notes);
}

#[test]
fn a_sync_waiting_for_another_leaves_the_reader_and_status_free() {
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let mut ask = ask(peer, slot.clone(), &tmp.0, false);
    let docs = lock(&slot.docs);
    let waiting = thread::spawn(move || ask.sync());
    thread::sleep(Duration::from_millis(100));
    assert!(slot.live.try_lock().is_ok(), "a sync holds Live only for a moment, never while it waits");
    notify(&slot, "textDocument/publishDiagnostics", json!({"uri": "file:///w/a.rs", "diagnostics": []}));
    drop(docs);
    assert_eq!(waiting.join().unwrap(), Vec::<PathBuf>::new());
}

#[test]
fn a_server_that_failed_to_load_says_so_and_status_shows_its_registrations() {
    let slot = Slot::default();
    notify(&slot, "experimental/serverStatus", json!({"health": "error", "quiescent": true, "message": "Failed to load workspaces.\n"}));
    lock(&slot.live).lsp.warm = true;
    assert_eq!(lock(&slot.live).lsp.index(), "warm (health error: Failed to load workspaces.)");
    let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch: 0, settings: settings(), folders: json!([]), log: None };
    let reg = json!({"registrations": [{"id": "1", "method": "workspace/didChangeWatchedFiles"}]});
    assert_eq!(reader.request("client/registerCapability", reg), Ok(Value::Null));
    let summary = lock(&slot.live).lsp.summary();
    assert_eq!(summary, ["index warm (health error: Failed to load workspaces.); 0 diagnostics", "registered workspace/didChangeWatchedFiles"]);
    let long = format!("cargo check failed to start: Cargo watcher failed\n    Updating crates.io index\n{}", "x".repeat(700));
    notify(&slot, "experimental/serverStatus", json!({"health": "warning", "quiescent": true, "message": long}));
    let live = lock(&slot.live);
    assert_eq!(live.lsp.index(), "warm (health warning: cargo check failed to start: Cargo watcher failed)", "the header gets its first line");
    let summary = live.lsp.summary();
    assert!(summary[1].starts_with("health warning: cargo check failed to start: Cargo watcher failed     Updating crates.io index xxx"), "status all of it: {summary:?}");
    assert_eq!(live.last.len(), 2, "each new health is logged once: {:?}", live.last);
    drop(live);
    notify(&slot, "experimental/serverStatus", json!({"health": "warning", "quiescent": true, "message": long}));
    assert_eq!(lock(&slot.live).last.len(), 2);
    notify(&slot, "experimental/serverStatus", json!({"health": "ok", "quiescent": true}));
    assert_eq!(lock(&slot.live).lsp.index(), "warm");
}

#[test]
fn did_save_carries_the_text_when_the_server_asks_for_it() {
    let (fake, peer, slot) =
        Fake::start(|method, _| if method == "initialize" { json!({"capabilities": {"textDocumentSync": {"change": 1, "save": {"includeText": true}}}}) } else { Value::Null });
    initialize(&peer, &slot, &Launch::default(), Path::new("/w"), json!([])).unwrap();
    assert!(lock(&slot.live).lsp.save_text);
    let tmp = TempDir::new("itse-lsp");
    let mut ask = ask(peer, slot, &tmp.0, false);
    let a = tmp.0.join("a.rs");
    fs::write(&a, "fn a() {}\n").unwrap();
    let uri = ask.open(&a).unwrap();
    fs::write(&a, "fn a() { 1 }\n").unwrap();
    ask.sync();
    let saved = fake.wait(6).into_iter().find(|m| m["method"] == "textDocument/didSave").expect("a didSave");
    assert_eq!(saved["params"], json!({"textDocument": {"uri": uri}, "text": "fn a() { 1 }\n"}));
}

#[test]
fn utf16_columns_on_a_non_ascii_line_become_byte_columns() {
    let line = "let s = \"é😀\"; s";
    assert_eq!((byte_col(line, 15, true), byte_col(line, 18, false)), (18, 18), "é is 1 unit and 2 bytes, 😀 2 units and 4 bytes");
    assert_eq!((byte_col(line, 10, true), byte_col(line, 11, true), byte_col(line, 99, true)), (11, 15, line.len()), "inside a surrogate pair it moves on");
    assert_eq!(byte_col(line, 10, false), 9, "a byte column inside a char moves back to its start");
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    fs::create_dir(tmp.0.join("src")).unwrap();
    fs::write(tmp.0.join("src/a.rs"), format!("// top\n{line}\n")).unwrap();
    let ask = ask(peer, slot.clone(), &tmp.0, false);
    let loc = json!({"uri": uri(&tmp.0.join("src/a.rs")), "range": {"start": {"line": 1, "character": 15}, "end": {"line": 1, "character": 16}}});
    lock(&slot.live).lsp.utf16 = true;
    let item = ask.item("X1".into(), "variable", &loc, "s".into());
    let l = item.loc.unwrap();
    assert_eq!((l.path.as_str(), l.line, l.col, item.src), ("src/a.rs", 2, 19, vec![(2, line.to_string())]));
    lock(&slot.live).lsp.utf16 = false;
    assert_eq!(ask.item("X1".into(), "variable", &loc, "s".into()).loc.unwrap().col, 16);
}

#[test]
fn a_location_without_a_file_keeps_its_uri_and_a_timeout_says_what_next() {
    let (_fake, peer, slot) = Fake::start(|_, _| Value::Null);
    let tmp = TempDir::new("itse-lsp");
    let ask = ask(peer, slot, &tmp.0, false);
    let jar = "jar:file:///lib/kotlin-stdlib.jar!/kotlin/collections/List.class";
    let item = ask.item(sid(1), "interface", &json!({"uri": jar, "range": {"start": {"line": 11, "character": 4}}}), "List".into());
    assert_eq!((item.loc.map(|l| (l.path, l.line, l.col)), item.src.len()), (Some((jar.to_string(), 12, 5)), 0));
    let e = anyhow!("lsp:fixture: timeout: no answer in 30 s (index busy?); itse status shows its progress");
    assert_eq!(err_class(&e), "timeout");
}

#[test]
fn uris_round_trip_with_spaces_and_non_ascii() {
    let path = Path::new("/w/a b/é#1.rs");
    assert_eq!(uri(path), "file:///w/a%20b/%C3%A9%231.rs");
    assert_eq!(path_of(&uri(path)).as_deref(), Some(path));
    assert_eq!(path_of("file:///w/a%3Ab.rs").as_deref(), Some(Path::new("/w/a:b.rs")));
    assert_eq!(path_of("untitled:1"), None);
    for java in ["file:/w/a.kt", "file://localhost/w/a.kt", "file:///w/a.kt"] {
        assert_eq!(path_of(java).as_deref(), Some(Path::new("/w/a.kt")), "{java}");
    }
    assert_eq!(path_of("file://host/w/a.kt"), None, "another host's file is no path here");
}

/// A crate in `root`, committed with its lock file: a module with a type error when `broken`.
fn ra_crate(root: &Path, broken: bool) -> bool {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"ra-ws\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n").unwrap();
    fs::write(root.join("src/lib.rs"), lib_rs(broken, 0)).unwrap();
    fs::write(root.join("src/extra.rs"), extra_rs(false, 0)).unwrap();
    let run = |mut cmd: Command| cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    let mut cargo = Command::new("cargo");
    cargo.args(["generate-lockfile", "--offline"]).current_dir(root);
    let git = |args: &[&str]| run(test_git(root, root, args));
    run(cargo) && git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-qm", "init"])
}

fn lib_rs(broken: bool, n: usize) -> String {
    format!("// edit {n}\npub mod extra;\n\npub fn add(a: u32, b: u32) -> u32 {{\n    a + {}\n}}\n", if broken { "\"b\"" } else { "b" })
}

fn extra_rs(broken: bool, n: usize) -> String {
    format!("// edit {n}\npub fn extra() -> u32 {{\n    {}\n}}\n", if broken { "\"x\"" } else { "3" })
}

/// The real thing runs whenever the ignored tests do, as the opt-in gate runs them; ITSE_E2E_RA=0 skips it.
pub fn ra_wanted() -> bool {
    let off = std::env::var("ITSE_E2E_RA").as_deref() == Ok("0");
    assert!(off || Command::new("rust-analyzer").arg("--version").output().is_ok_and(|o| o.status.success()), "rust-analyzer is not on PATH; ITSE_E2E_RA=0 skips this");
    !off
}

/// The time from an edit to the first push for the edited file, and to the first that says what
/// the edit did, at the client: for an opened document, and for one no query opened, which the sync
/// finds in `git status`, opens and saves, as a hook's call does (the e2e test times the whole hook
/// call). rust-analyzer runs from PATH as the built-in `rust` extension declares it, trusted, in a
/// private HOME; p50 and p95 over 20 edits each are printed and appended to target/budget/history.jsonl.
#[test]
#[ignore = "needs rust-analyzer and cargo on PATH"]
fn rust_analyzer_pushes_diagnostics_soon_after_an_edit() {
    if !ra_wanted() {
        return eprintln!("skipped: ITSE_E2E_RA=0");
    }
    let tmp = TempDir::new("itse-ra");
    let root = tmp.0.join("work");
    assert!(ra_crate(&root, false), "cargo and git make the crate");
    let vars: BTreeMap<&str, std::ffi::OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = crate::workspace::Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let ws = crate::workspace::Workspace::find(Some(root.clone()), &root).unwrap();
    let args = crate::verbs::Args { target: Some("rust".into()), ..Default::default() };
    crate::manifest::use_ext(&ws, &dirs, &args, true).unwrap();
    // The server's env is the test's with a private HOME, so cargo's own dirs are private too.
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    env.insert("HOME".into(), tmp.0.join("home").display().to_string());
    let r = resolve::resolve(&ws, &dirs, &env, SystemTime::now()).unwrap();
    crate::lock::trust(&dirs, &ws, &r.trust_hash, false, "tty").unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    let mut host = Host::new(ws, dirs, socket, (0, 0));
    host.base_env = env.iter().map(|(k, v)| (k.into(), v.into())).collect();
    let host = Arc::new(host);
    let mut ask = servers(&host, &env, "test", "", &|_| true, true, &Cancel::default()).unwrap_or_else(|o| panic!("{}", o.head())).asks.remove(0);
    let lib = root.join("src/lib.rs");
    ask.open(&lib).unwrap();
    let started = Instant::now();
    while !ask.settle(Instant::now() + Duration::from_secs(5), &Cancel::default()) {
        assert!(started.elapsed() < Duration::from_secs(300), "rust-analyzer never settled: {}", ask.index());
    }
    eprintln!("rust-analyzer settled {:.1} s after the start: index {}", started.elapsed().as_secs_f64(), ask.index());
    let (opened, unopened) = (pushes(&mut ask, &lib, false), pushes(&mut ask, &root.join("src/extra.rs"), true));
    let pct = |v: &[u64], p: usize| {
        let mut v = v.to_vec();
        v.sort_unstable();
        v[(v.len() * p).div_ceil(100).max(1) - 1]
    };
    let mut line = json!({"at": sys::utc(SystemTime::now()), "gate": "ra-push"});
    for (path, (times, errors)) in [("opened", &opened), ("unopened", &unopened)] {
        let (p50, p95, e50, e95) = (pct(times, 50), pct(times, 95), pct(errors, 50), pct(errors, 95));
        eprintln!("rust-analyzer edit -> first push, {path} file: p50 {p50} ms, p95 {p95} ms over {} edits: {times:?}", times.len());
        eprintln!("rust-analyzer edit -> first push that says what it did, {path} file: p50 {e50} ms, p95 {e95} ms: {errors:?}");
        line[path] = json!({"p50_ms": p50, "p95_ms": p95, "error_p50_ms": e50, "error_p95_ms": e95, "ms": times, "error_ms": errors});
    }
    let _ = fs::create_dir_all("target/budget");
    let _ = OpenOptions::new().create(true).append(true).open("target/budget/history.jsonl").and_then(|mut f| writeln!(f, "{line}"));
    let target = host.dirs.cache.join("rust-analyzer").join(&host.ws.ws);
    host.stop_servers(Duration::from_secs(5));
    assert!(target.is_dir(), "cargo check built under the cache: {}", target.display());
    assert!(!root.join("target").exists(), "and not in the workspace");
}

/// 20 edits of `file`, broken and fixed in turn, each told by a sync: the milliseconds to the first
/// push for it, and to the first that says what the edit did. `close` closes it after each, so
/// every edit reaches the server as one to a file no query opened.
fn pushes(ask: &mut Ask, file: &Path, close: bool) -> (Vec<u64>, Vec<u64>) {
    let (mut times, mut errors) = (Vec::new(), Vec::new());
    for n in 1..=20 {
        let broken = n % 2 == 1;
        fs::write(file, if close { extra_rs(broken, n) } else { lib_rs(broken, n) }).unwrap();
        let (edited, since) = (Instant::now(), lock(&ask.slot.live).lsp.seq);
        assert_eq!(ask.sync(), [file.to_path_buf()]);
        let mut live = lock(&ask.slot.live);
        while !live.lsp.diags.iter().any(|(u, d)| d.seq > since && path_of(u).as_deref() == Some(file)) {
            assert!(edited.elapsed() < Duration::from_secs(60), "no push 60 s after edit {n}");
            live = ask.slot.woke.wait_timeout(live, Duration::from_millis(20)).unwrap_or_else(PoisonError::into_inner).0;
        }
        times.push(edited.elapsed().as_millis() as u64);
        let right = |live: &Live| {
            let d = live.lsp.diags.iter().find(|(u, _)| path_of(u).as_deref() == Some(file)).map(|(_, d)| d);
            d.is_some_and(|d| d.seq > since && d.items.iter().any(|i| i.severity == 1) == broken)
        };
        while !right(&live) {
            assert!(edited.elapsed() < Duration::from_secs(60), "no push 60 s after edit {n} says what it did");
            live = ask.slot.woke.wait_timeout(live, Duration::from_millis(20)).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(live);
        errors.push(edited.elapsed().as_millis() as u64);
        // The edit's cargo check finishes before the next edit, so each push measured is that edit's first.
        ask.settle(Instant::now() + Duration::from_secs(60), &Cancel::default());
        if close {
            let mut docs = lock(&ask.slot.docs);
            docs.open.retain(|d| d.path != file || super::close(&ask.peer, d));
        }
    }
    (times, errors)
}

#[test]
fn a_qualifier_fits_the_last_container_segment_then_the_container_or_the_path() {
    assert_eq!(segments("data::Region.save"), ["data", "Region", "save"]);
    assert_eq!(segments("com.acme.Order#save"), ["com", "acme", "Order", "save"]);
    let of = |c: Value| container(&json!({"name": "save", "containerName": c}));
    let (method, free) = (of(json!("Region")), of(Value::Null));
    let (region, orders) = (Path::new("src/data/region.rs"), Path::new("src/orders.rs"));
    assert!(qualified(&["data", "Region"], &method, region) && qualified(&["Region"], &method, region) && qualified(&[], &method, region));
    assert!(!qualified(&["Other"], &method, region) && !qualified(&["x", "Region"], &method, region) && !qualified(&["data"], &method, region));
    assert!(qualified(&["orders"], &free, orders) && qualified(&["src", "orders"], &free, orders) && qualified(&["REGION"], &free, region), "a path component or stem in any case");
    let kotlin = of(json!("com.acme.data.OrderRepository"));
    let kt = Path::new("src/main/kotlin/com/acme/data/OrderRepository.kt");
    for quals in [&["data", "OrderRepository"][..], &["acme", "OrderRepository"], &["com", "acme", "data", "OrderRepository"], &["main", "OrderRepository"]] {
        assert!(qualified(quals, &kotlin, kt), "{quals:?}");
    }
    assert!(!qualified(&["OrderRepository", "data"], &kotlin, Path::new("x.kt")), "the segment before the name is the container's last");
    // rust-analyzer keeps an impl's generic arguments in its container.
    assert_eq!((of(json!("Wrapper<T>")), of(json!("Foo<std::io::Error, Vec<u8>>"))), (vec!["Wrapper".to_string()], vec!["Foo".to_string()]));
    assert!(qualified(&["data", "Wrapper"], &of(json!("Wrapper<'a, T>")), Path::new("src/data.rs")));
    assert_eq!((kinds(Some("method")), kinds(Some("struct")), kinds(None)), (Ok(vec![6, 12]), Ok(vec![23]), Ok(vec![])));
    assert!(kinds(Some("bogus")).unwrap_err().starts_with("unknown kind bogus; one of file, module, "));
}

#[test]
fn a_byte_column_becomes_the_servers_column_and_back() {
    let line = "let é = \"𝄞\"; x";
    let x = line.find('x').unwrap();
    assert_eq!((server_col(line, x, false), server_col(line, x, true)), (x, x - 1 - 2));
    assert_eq!(byte_col(line, server_col(line, x, true) as u32, true), x);
    assert_eq!(server_col(line, 5, true), server_col(line, 4, true), "inside a char counts from its start");
}

#[test]
fn a_server_without_a_command_fails_before_its_logs_turn_over() {
    let tmp = TempDir::new("itse-lsp");
    let vars: BTreeMap<&str, std::ffi::OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = crate::workspace::Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let ws = crate::workspace::Workspace::find(Some(tmp.0.clone()), &tmp.0).unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    let host = Host::new(ws, dirs, socket, (0, 0));
    let dir = host.ws.state_dir(&host.dirs).join("servers");
    sys::private_dir(&dir).unwrap();
    fs::write(dir.join("fixture.log"), "last real log\n").unwrap();
    fs::write(dir.join("fixture.log.1"), "older log\n").unwrap();
    let failed = start(&host, &Slot::default(), &Launch { id: "fixture".into(), ..Launch::default() }, &BTreeMap::new()).err().map(|e| e.to_string());
    assert_eq!(failed.as_deref(), Some("fixture has no command"));
    let logs = (fs::read_to_string(dir.join("fixture.log")).ok(), fs::read_to_string(dir.join("fixture.log.1")).ok());
    assert_eq!(logs, (Some("last real log\n".into()), Some("older log\n".into())), "the logs stay as they were");
}
