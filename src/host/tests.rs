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

fn up() -> (Arc<Peer>, Child) {
    let peer = Peer::start(io::empty(), io::sink(), Framing::Lines, Dialect::Mcp, Arc::new(Quiet));
    (peer, spawn(Command::new("sleep").arg("30"), &Limits::default(), false, 800).unwrap())
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
                slot.up(start).map(|(peer, note)| (Arc::as_ptr(&peer) as usize, note)).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = callers.into_iter().map(|c| c.join().unwrap()).collect();
    assert_eq!(starts.load(Ordering::SeqCst), 1, "one start for three callers");
    assert!(results.iter().all(|r| *r == results[0]), "they share one peer: {results:?}");
    assert!(slot.state().starts_with("running (pid "), "{}", slot.state());
    assert!(slot.stop(Duration::from_secs(1)));
    assert_eq!((slot.state(), slot.stop(Duration::from_secs(1))), ("stopped".into(), false));
}

#[test]
fn a_slow_start_shows_as_starting_and_its_waiters_get_its_failure() {
    let slot = Arc::new(Slot::default());
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map_err(anyhow::Error::from).and_then(|()| bail!("exit 1 before initialize"))).map(|_| ()))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    let second = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| panic!("a waiter never starts a second copy")).map(|_| ()))
    };
    thread::sleep(Duration::from_millis(50));
    assert_eq!(slot.state(), "starting (0 s)", "status answers while a start runs");
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap_err().to_string(), "start_failed");
    assert_eq!(second.join().unwrap().unwrap_err().to_string(), "start_failed: exit 1 before initialize");
    assert_eq!(slot.state(), "crashed: exit 1 before initialize");
    let (_, note) = slot.up(|| Ok(up())).unwrap();
    assert_eq!(note.as_deref(), Some("restarted after crash (exit 1 before initialize)"), "the next use starts it again");
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
    assert_eq!(Crash::new(ExitStatus::from_raw(9), &codes, &log).text, "killed by signal 9 (likely out of memory)");
    assert_eq!(Crash::new(ExitStatus::from_raw(11), &codes, &log).text, "killed by signal 11");
    let slot = Slot::default();
    *lock(&slot.phase) = Phase::Crashed(Crash::new(ExitStatus::from_raw(9), &codes, &log));
    assert_eq!(slot.up(|| Ok(up())).unwrap().1.as_deref(), Some("restarted after crash (killed by signal 9 (likely out of memory))"));
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
    assert_eq!((slot.stop(Duration::from_secs(1)), slot.state()), (false, "gone: daemon restarted".into()));
    *lock(&slot.phase) = Phase::Crashed(Crash { text: "exit 7".into(), ..Crash::default() });
    assert_eq!((slot.stop(Duration::from_secs(1)), slot.state()), (false, "crashed: exit 7".into()));
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map_err(anyhow::Error::from).map(|()| up())).map(|(_, note)| note))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!slot.stop(Duration::from_secs(1)));
    assert!(slot.state().starts_with("starting ("), "a stop during a start lets no second caller start another copy: {}", slot.state());
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap().as_deref(), Some("restarted after crash (exit 7)"));
    assert!(slot.stop(Duration::from_secs(1)));
}

#[test]
fn a_start_that_panics_leaves_the_slot_crashed_and_wakes_its_waiters() {
    let slot = Arc::new(Slot::default());
    let (release, released) = mpsc::channel::<()>();
    let first = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| released.recv().map(|()| panic!("boom")).map_err(anyhow::Error::from)).map(|_| ()))
    };
    while !slot.state().starts_with("starting (") {
        thread::sleep(Duration::from_millis(5));
    }
    let second = {
        let slot = slot.clone();
        thread::spawn(move || slot.up(|| panic!("a waiter never starts a second copy")).map(|_| ()))
    };
    thread::sleep(Duration::from_millis(50));
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap_err().to_string(), "start_failed");
    assert_eq!(second.join().unwrap().unwrap_err().to_string(), "start_failed: start panicked");
    assert_eq!(slot.state(), "crashed: start panicked");
    let (_, note) = slot.up(|| Ok(up())).unwrap();
    assert_eq!(note.as_deref(), Some("restarted after crash (start panicked)"), "the next use starts it again");
    slot.stop(Duration::from_secs(1));
}
