use serde_json::json;

use super::*;

#[test]
fn limits_come_from_extension_data_but_the_oom_score_does_not() {
    let limits: Limits = serde_json::from_str(r#"{"nofile": 4096, "as_mb": 2048}"#).unwrap();
    assert_eq!(limits, Limits { nofile: Some(4096), as_mb: Some(2048), cpu_s: None, oom_score_adj: 0 });
    assert!(serde_json::from_str::<Limits>(r#"{"oom_score_adj": -1000}"#).is_err());
}

#[test]
fn next_numbers_are_never_reused() {
    let tmp = TempDir::new("itse-host");
    assert_eq!((Next::take(&tmp.0, "run").unwrap(), Next::take(&tmp.0, "q").unwrap(), Next::take(&tmp.0, "run").unwrap()), ("run:1".into(), "q:1".into(), "run:2".into()));
    let next: Next = serde_json::from_str(r#"{"run": 14}"#).unwrap();
    assert_eq!(next, Next { run: 14, dbg: 0, q: 0 });
}

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;

use crate::rpc::{Dialect, Framing, Handler};
use crate::sys::tests::TempDir;

/// Field `n` (1-based, as in proc(5)) of /proc/<pid>/stat.
fn stat_field(pid: u32, n: usize) -> String {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    text[text.rfind(')').unwrap() + 2..].split(' ').nth(n - 3).unwrap().to_string()
}

fn limit(pid: u32, name: &str) -> String {
    let limits = fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    limits.lines().find(|l| l.starts_with(name)).unwrap()[26..].split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

#[test]
fn a_spawned_server_leads_its_group_with_its_limits_and_oom_score() {
    let limits = Limits { nofile: Some(77), cpu_s: Some(600), ..Limits::default() };
    let mut child = spawn(Command::new("sleep").arg("30"), &limits, false, 800).unwrap();
    let pid = child.id();
    assert_eq!(stat_field(pid, 5), pid.to_string(), "pgid == pid");
    assert_ne!(stat_field(pid, 6), pid.to_string(), "a server keeps the daemon's session");
    assert_eq!((limit(pid, "Max open files"), limit(pid, "Max core file size"), limit(pid, "Max cpu time")), ("77 77".into(), "0 0".into(), "600 600".into()));
    assert_eq!(fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap().trim(), "800");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_runner_leads_its_own_session_and_gets_oom_score_500() {
    let mut child = spawn(Command::new("sleep").arg("30"), &Limits::default(), true, 500).unwrap();
    let pid = child.id();
    assert_eq!((stat_field(pid, 5), stat_field(pid, 6)), (pid.to_string(), pid.to_string()));
    assert_eq!(fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap().trim(), "500");
    assert_eq!(limit(pid, "Max open files"), limit(std::process::id(), "Max open files"), "an undeclared limit is inherited");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn the_base_env_keeps_the_parents_vars_but_not_itse_claude_or_ai_agent() {
    let vars = ["PATH", "HOME", "ITSE_SESSION", "CLAUDE_CODE_SESSION_ID", "AI_AGENT", "AI_AGENTS", "MY_ITSE_X"].map(|k| (OsString::from(k), OsString::from("v")));
    let kept: Vec<OsString> = base_env(vars.into_iter()).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kept, ["PATH", "HOME", "AI_AGENTS", "MY_ITSE_X"]);
}

struct Quiet;
impl Handler for Quiet {}

fn up() -> Server {
    // The peer reads the child's stdout, so its stream stays open while the child lives.
    let mut child = spawn(Command::new("sleep").arg("30").stdout(std::process::Stdio::piped()), &Limits::default(), false, 800).unwrap();
    let peer = Peer::start(io::BufReader::new(child.stdout.take().unwrap()), io::sink(), Framing::Lines, Dialect::Mcp, Arc::new(Quiet));
    Server { peer, child: Mutex::new(child), exit_codes: BTreeMap::new(), log: PathBuf::new(), idle: Duration::ZERO, hash: String::new(), marker: String::new(), epoch: 0 }
}

#[test]
fn concurrent_callers_share_one_start() {
    let (slot, starts) = (Arc::new(Slot::default()), Arc::new(AtomicUsize::new(0)));
    let callers: Vec<_> = (0..3)
        .map(|_| {
            let (slot, starts) = (slot.clone(), starts.clone());
            thread::spawn(move || {
                let start = || {
                    starts.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(200));
                    Ok(up())
                };
                slot.up(start, &Cancel::default()).map(|(peer, note, fresh)| (Arc::as_ptr(&peer) as usize, note, fresh)).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
    assert_eq!(starts.load(Ordering::SeqCst), 1, "one start for three callers");
    assert!(results.iter().all(|r| *r == results[0]), "they share one peer: {results:?}");
    assert!(results[0].2, "each caller saw the start, so each says its index is cold");
    assert!(slot.state().starts_with("up (pid "), "{}", slot.state());
    assert!(slot.stop(Duration::from_secs(1)).is_some());
    assert_eq!((slot.state(), slot.stop(Duration::from_secs(1)).is_some()), ("stopped".into(), false));
}

#[test]
fn a_slow_start_shows_as_starting_and_its_waiters_get_its_failure() {
    let slot = Arc::new(Slot::default());
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map_err(anyhow::Error::from).and_then(|()| bail!("exit 1 before initialize")), &Cancel::default()).map(|_| ()))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    let second = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| panic!("a waiter never starts a second copy"), &Cancel::default()).map(|_| ()))
    };
    thread::sleep(Duration::from_millis(50));
    assert_eq!(slot.state(), "starting (0 s)", "status answers while a start runs");
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap_err().to_string(), "start_failed");
    assert_eq!(second.join().unwrap().unwrap_err().to_string(), "start_failed: exit 1 before initialize");
    assert_eq!(slot.state(), "crashed: exit 1 before initialize");
    let (_, note, _) = slot.up(|| Ok(up()), &Cancel::default()).unwrap();
    assert_eq!(note.as_deref(), Some("restarted after a crash (exit 1 before initialize)"), "the next use starts it again");
    slot.stop(Duration::from_secs(1));
}

#[test]
fn a_crash_says_its_exit_its_mapped_reason_or_its_signal() {
    let tmp = TempDir::new("itse-host");
    let log = tmp.0.join("x.log");
    fs::write(&log, "a\n\nb\nc\nd\ne\nf\n").unwrap();
    let codes: BTreeMap<i32, String> = [(7, "server_expired".to_string())].into();
    let expired = Crash::new(ExitStatus::from_raw(7 << 8), &codes, &log);
    assert_eq!((expired.text.clone(), expired.stderr_tail.clone()), ("exit 7, server_expired".into(), ["b", "c", "d", "e", "f"].map(String::from).to_vec()));
    assert_eq!(Crash::new(ExitStatus::from_raw(2 << 8), &codes, &log).text, "exit 2");
    assert_eq!(Crash::new(ExitStatus::from_raw(9), &codes, &log).text, "killed by signal 9 (out of memory, or killed)");
    assert_eq!(Crash::new(ExitStatus::from_raw(11), &codes, &log).text, "killed by signal 11");
    fs::write(&log, format!("{}\nlast\n", "x".repeat(1 << 20))).unwrap();
    assert_eq!(tail(&log, 1), ["last"], "a big log is read from its end");
    let slot = Slot::default();
    *lock(&slot.phase) = Phase::Crashed(Crash::new(ExitStatus::from_raw(9), &codes, &log));
    assert_eq!(slot.up(|| Ok(up()), &Cancel::default()).unwrap().1.as_deref(), Some("restarted after a crash (killed by signal 9 (out of memory, or killed))"));
    slot.stop(Duration::from_secs(1));
}

fn host(tmp: &TempDir) -> Arc<Host> {
    let vars: BTreeMap<&str, OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let ws = Workspace::find(Some(tmp.0.clone()), &tmp.0).unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    Arc::new(Host::new(ws, dirs, socket, (1, 2)))
}

#[test]
fn a_restart_kills_leftover_servers_ignores_a_reused_pid_and_ends_dead_runs() {
    let tmp = TempDir::new("itse-host");
    let host = host(&tmp);
    let state = host.ws.state_dir(&host.dirs);
    sys::private_dir(&state.join("runs/4")).unwrap();
    fs::write(state.join("runs/4/run.json"), r#"{"spec": {"label": "t:dev"}, "pid": {"runner_pid": 1, "child_pid": 2, "start_time": 3}}"#).unwrap();
    let sleeper = || spawn(Command::new("sleep").arg("30"), &Limits::default(), false, 800).unwrap();
    let (mut ours, mut reused) = (sleeper(), sleeper());
    let start = |c: &Child| sys::proc_stat(c.id() as i32).unwrap().start_time;
    let record = |handle: &str, kind: &str, c: &Child, start: u64| json!({"handle": handle, "kind": kind, "pid": c.id(), "pgid": c.id(), "start_time": start});
    let procs = [
        record("lsp:a", "lsp", &ours, start(&ours)),
        record("mcp:b", "mcp", &reused, start(&reused) + 1),
        json!({"handle": "run:4", "kind": "run", "pid": 999_999_999, "start_time": 3}),
    ];
    fs::write(state.join("procs.json"), serde_json::to_vec(&procs).unwrap()).unwrap();
    host.restart();
    assert_eq!(ours.wait().unwrap().signal(), Some(sys::SIGTERM), "the server left from the old daemon is killed");
    assert_eq!(reused.try_wait().unwrap(), None, "a pid with another start time is not ours");
    assert_eq!((host.slot("lsp:a").state(), host.slot("mcp:b").state()), ("gone: daemon restarted".to_string(), "gone: daemon restarted".to_string()));
    let run: Value = serde_json::from_slice(&fs::read(state.join("runs/4/run.json")).unwrap()).unwrap();
    assert_eq!((run["exit"]["signal"].as_i64(), run["exit"]["cancelled"].as_bool()), (Some(9), Some(true)), "a runner gone without its exit was killed");
    assert!(state.join("runs/4/result.json").exists() && lock(&host.runs).is_empty());
    assert_eq!(fs::read_to_string(state.join("procs.json")).unwrap(), "[]");
    reused.kill().unwrap();
    reused.wait().unwrap();
}

#[test]
fn stopping_a_slot_that_is_not_up_keeps_its_phase() {
    let slot = Arc::new(Slot::default());
    *lock(&slot.phase) = Phase::Gone("daemon restarted");
    assert_eq!((slot.stop(Duration::from_secs(1)).is_some(), slot.state()), (false, "gone: daemon restarted".into()));
    *lock(&slot.phase) = Phase::Crashed(Crash { text: "exit 7".into(), ..Crash::default() });
    assert_eq!((slot.stop(Duration::from_secs(1)).is_some(), slot.state()), (false, "crashed: exit 7".into()));
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map_err(anyhow::Error::from).map(|()| up()), &Cancel::default()).map(|(_, note, _)| note))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(slot.stop(Duration::from_secs(1)).is_none());
    assert!(slot.state().starts_with("starting ("), "a stop during a start lets no second caller start another copy: {}", slot.state());
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap().as_deref(), Some("restarted after a crash (exit 7)"));
    assert!(slot.stop(Duration::from_secs(1)).is_some());
}

#[test]
fn a_start_that_panics_leaves_the_slot_crashed_and_wakes_its_waiters() {
    let slot = Arc::new(Slot::default());
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map(|()| panic!("boom")).map_err(anyhow::Error::from), &Cancel::default()).map(|_| ()))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    let second = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| panic!("a waiter never starts a second copy"), &Cancel::default()).map(|_| ()))
    };
    thread::sleep(Duration::from_millis(50));
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap_err().to_string(), "start_failed");
    assert_eq!(second.join().unwrap().unwrap_err().to_string(), "start_failed: start panicked");
    assert_eq!(slot.state(), "crashed: start panicked");
    let (_, note, _) = slot.up(|| Ok(up()), &Cancel::default()).unwrap();
    assert_eq!(note.as_deref(), Some("restarted after a crash (start panicked)"), "the next use starts it again");
    slot.stop(Duration::from_secs(1));
}

#[test]
fn a_crashed_server_takes_what_it_started_with_it() {
    let tmp = TempDir::new("itse-host");
    let left = tmp.0.join("left");
    // The leader exits and leaves a child in its group, as a server dying under a build would.
    let script = format!("sleep 30 & echo $! > {}; exit 3", left.display());
    let mut child = spawn(Command::new("sh").args(["-c", &script]).stdout(std::process::Stdio::piped()), &Limits::default(), false, 800).unwrap();
    let peer = Peer::start(io::BufReader::new(child.stdout.take().unwrap()), io::sink(), Framing::Lines, Dialect::Mcp, Arc::new(Quiet));
    let server =
        Server { peer, child: Mutex::new(child), exit_codes: BTreeMap::new(), log: PathBuf::new(), idle: Duration::ZERO, hash: String::new(), marker: String::new(), epoch: 0 };
    while lock(&server.child).try_wait().unwrap().is_none() {
        thread::sleep(Duration::from_millis(10));
    }
    let sleeper: i32 = fs::read_to_string(&left).unwrap().trim().parse().unwrap();
    let mut phase = Phase::Up(Arc::new(server));
    assert!(Slot::crashed(&mut phase).is_some());
    assert!(matches!(&phase, Phase::Crashed(c) if c.text == "exit 3"));
    let end = Instant::now() + Duration::from_secs(2);
    while sys::proc_stat(sleeper).is_some_and(|s| s.alive()) && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!sys::proc_stat(sleeper).is_some_and(|s| s.alive()), "the group's leftover member is killed");
}

#[test]
fn an_idle_stop_spares_a_held_server_and_a_new_servers_record() {
    let tmp = TempDir::new("itse-host");
    let host = host(&tmp);
    sys::private_dir(&host.ws.state_dir(&host.dirs)).unwrap();
    let slot = host.slot("lsp:a");
    let server = Arc::new(Server { idle: Duration::from_millis(1), ..up() });
    *lock(&slot.phase) = Phase::Up(server.clone());
    thread::sleep(Duration::from_millis(5));
    host.reap();
    assert!(slot.state().starts_with("up (pid "), "a query holds it: {}", slot.state());
    let pid = lock(&server.child).id() as i32;
    drop(server);
    host.track("lsp:a", Some(json!({"handle": "lsp:a", "kind": "lsp", "pid": pid})));
    host.reap();
    assert_eq!(slot.state(), "stopped", "taken out of the slot at once");
    // A new start records itself before the old stop is done; the old stop drops its own record only.
    host.track("lsp:a", Some(json!({"handle": "lsp:a", "kind": "lsp", "pid": 1})));
    let procs = host.ws.state_dir(&host.dirs).join("procs.json");
    let records = || serde_json::from_slice::<Value>(&fs::read(&procs).unwrap()).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while sys::proc_stat(pid).is_some_and(|s| s.alive()) && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(records(), json!([{"handle": "lsp:a", "kind": "lsp", "pid": 1}]));
}

#[test]
fn a_new_record_of_a_handle_keeps_the_one_of_another_pid() {
    let tmp = TempDir::new("itse-host");
    let host = host(&tmp);
    sys::private_dir(&host.ws.state_dir(&host.dirs)).unwrap();
    let rec = |pid: i32, log: &str| json!({"handle": "lsp:a", "kind": "lsp", "pid": pid, "log": log});
    let procs = host.ws.state_dir(&host.dirs).join("procs.json");
    let records = || serde_json::from_slice::<Value>(&fs::read(&procs).unwrap()).unwrap();
    host.track("lsp:a", Some(rec(10, "")));
    host.track("lsp:a", Some(rec(11, "")));
    host.track("lsp:a", Some(rec(11, "x")));
    assert_eq!(records(), json!([rec(10, ""), rec(11, "x")]), "a server still stopping keeps its record; one pid's is replaced");
    host.untrack("lsp:a", 10);
    assert_eq!(records(), json!([rec(11, "x")]));
    host.track("lsp:a", None);
    assert_eq!(records(), json!([]));
}

#[test]
fn get_gives_an_up_server_and_never_starts_one() {
    let slot = Slot::default();
    assert!(matches!(slot.get(), (None, None)), "stopped stays stopped");
    let server = Arc::new(up());
    let pid = lock(&server.child).id() as i32;
    *lock(&slot.phase) = Phase::Up(server.clone());
    assert!(slot.get().0.is_some_and(|s| Arc::ptr_eq(&s, &server)));
    sys::send_signal(pid, sys::SIGKILL);
    let end = Instant::now() + Duration::from_secs(5);
    let ended = loop {
        match slot.get() {
            (None, ended) => break ended,
            _ if Instant::now() > end => panic!("still up"),
            _ => thread::sleep(Duration::from_millis(20)),
        }
    };
    assert_eq!(ended, Some(pid), "an ended one is crashed, with its pid for procs.json");
    assert!(slot.state().starts_with("crashed: killed by signal 9"), "{}", slot.state());
}

#[test]
fn a_server_log_past_4_mb_starts_afresh() {
    let tmp = TempDir::new("itse-host");
    let host = host(&tmp);
    let log = tmp.0.join("s.log");
    fs::write(&log, vec![b'x'; MAX_LOG as usize + 1]).unwrap();
    let slot = host.slot("lsp:a");
    *lock(&slot.phase) = Phase::Up(Arc::new(Server { log: log.clone(), ..up() }));
    host.reap();
    assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    slot.stop(Duration::from_secs(1));
}
