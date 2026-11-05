//! The session host: the daemon's shared state (`Host`), its registry of
//! server slots and live runs, `procs.json`, how children are spawned (own process group, rlimits,
//! OOM score), what a crash leaves, and the clean-up after a daemon restart.
//!
//! A slot has two parts, so `status` never hangs on a start: `phase`, behind a Mutex and a Condvar,
//! which one caller moves to `Starting` and then starts the server with no lock held while the
//! others wait; and `live`, which only the server's reader thread fills, in short locks. Every lock
//! goes through `sys::lock`, so a panic that poisons one can't disable a server for good.

// Servers will start through the registry too; for now only runs use it.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::{env, ffi::OsString, fs, io, mem, panic, panic::AssertUnwindSafe, path::Path, path::PathBuf, time::Duration, time::Instant, time::SystemTime};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::actions::{self, Run};
use crate::{manifest::Config, manifest::DapVia, rpc::Peer, sys, sys::lock, usage::Usage, workspace::Dirs, workspace::Workspace};

/// One per daemon, shared by every connection.
pub struct Host {
    pub ws: Workspace,
    pub dirs: Dirs,
    pub socket: PathBuf,
    pub started: SystemTime,
    /// `cli` or `mcp`: who started this daemon, shown by `status`.
    pub started_by: String,
    /// `(dev, inode)` of the binary this daemon runs, for the CLI's replaced-binary check.
    pub exe: (u64, u64),
    /// The starter's env minus `ITSE_*`, `CLAUDE_*` and `AI_AGENT`: the base env of servers and runs.
    pub base_env: Vec<(OsString, OsString)>,
    /// The daemon exits after this long with no connection.
    pub idle: Duration,
    conns: AtomicUsize,
    /// MCP sessions attached now. While one is, `hello` says `busy`, so a CLI from a replaced binary
    /// is served here instead of shutting down the session's daemon.
    sessions: AtomicUsize,
    last: Mutex<Instant>,
    pub stopping: AtomicBool,
    /// The inode of the socket file this daemon bound last. Binding and stopping both hold it, so a
    /// stopping daemon never re-binds.
    pub bound: Mutex<u64>,
    pub usage: Usage,
    /// `lsp:<id>` and `mcp:<id>` → their slot; a handle names the same server across restarts.
    pub slots: Mutex<BTreeMap<String, Arc<Slot>>>,
    /// `run:N` → the runs that haven't ended, started here or re-adopted.
    pub runs: Mutex<BTreeMap<String, Arc<Run>>>,
    /// What `procs.json` holds: one record per live child, rewritten on every change.
    procs: Mutex<Vec<Value>>,
}

impl Host {
    /// The idle time is `ITSE_DAEMON_IDLE_S`, else `daemon_idle_minutes` (5); the starter is named
    /// by `ITSE_STARTED_BY`, which `__daemonize` sets.
    pub fn new(ws: Workspace, dirs: Dirs, socket: PathBuf, exe: (u64, u64)) -> Self {
        let config = config(&dirs);
        let idle = env::var("ITSE_DAEMON_IDLE_S").ok().and_then(|s| s.parse().ok()).unwrap_or(config.daemon_idle_minutes.unwrap_or(5) * 60);
        let started_by = env::var("ITSE_STARTED_BY").ok().filter(|b| b == "mcp").unwrap_or_else(|| "cli".into());
        let (conns, last, stopping, bound) = (AtomicUsize::new(0), Mutex::new(Instant::now()), AtomicBool::new(false), Mutex::new(0));
        let (usage, sessions, base_env) = (Usage::new(&dirs, &ws.root, true), AtomicUsize::new(0), base_env(env::vars_os()));
        let (slots, runs, procs, started, idle) = (Mutex::default(), Mutex::default(), Mutex::default(), SystemTime::now(), Duration::from_secs(idle));
        Self { ws, dirs, socket, started, started_by, exe, base_env, idle, conns, sessions, last, stopping, bound, usage, slots, runs, procs }
    }

    /// A handle's slot, made `Stopped` on first use.
    pub fn slot(&self, handle: &str) -> Arc<Slot> {
        lock(&self.slots).entry(handle.into()).or_default().clone()
    }

    /// Add a child to `procs.json` (`Some`), or drop `handle` from it; written atomically each time.
    pub fn track(&self, handle: &str, record: Option<Value>) {
        let mut procs = lock(&self.procs);
        procs.retain(|p| p["handle"] != handle);
        procs.extend(record);
        let _ = sys::write_atomic(&self.ws.state_dir(&self.dirs).join("procs.json"), &serde_json::to_vec(&*procs).unwrap_or_default())
            .inspect_err(|e| eprintln!("itse daemon: procs.json: {e:#}"));
    }

    /// After a daemon restart, from the old `procs.json`: runs are re-adopted (or their end written);
    /// servers and adapters lost their pipes, so what is left of their groups is killed and their
    /// handles say `gone: daemon restarted`. A pid counts only with its recorded start time and not as
    /// a zombie; with the leader gone, the group's leftover members still go, since Linux reuses no
    /// pid that is still a group id.
    pub fn restart(self: &Arc<Self>) {
        let old: Vec<Value> = fs::read(self.ws.state_dir(&self.dirs).join("procs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let (runs, servers): (Vec<Value>, Vec<Value>) = old.into_iter().partition(|p| p["kind"] == "run");
        self.track("", None);
        runs.into_iter().for_each(|p| actions::adopt(self, p));
        for p in servers {
            let n = |k: &str| p[k].as_i64().unwrap_or_default();
            let (pid, pgid, start) = (n("pid") as i32, n("pgid") as i32, n("start_time") as u64);
            if sys::alive(pid, start) || (!sys::proc_stat(pid).is_some_and(|s| s.alive()) && pgid > 1 && sys::send_signal(-pgid, 0)) {
                sys::kill_group(pgid, Duration::from_secs(1), || ());
            }
            *lock(&self.slot(p["handle"].as_str().unwrap_or_default()).phase) = Phase::Gone("daemon restarted");
        }
    }

    /// `config.json`, read again on each use, so an edit applies to the next call.
    pub fn config(&self) -> Config {
        config(&self.dirs)
    }

    /// The first 12 hex digits of sha256 over the base env's sorted key names: a daemon started
    /// from another env shows another value.
    pub fn env_keys_sha(&self) -> String {
        let mut keys: Vec<&[u8]> = self.base_env.iter().map(|(k, _)| k.as_encoded_bytes()).collect();
        keys.sort();
        sys::sha256_hex(&keys.join(&b'\n'))[..12].to_string()
    }

    /// Count a connection until the guard drops; an open one keeps the daemon alive.
    pub fn enter(self: &Arc<Self>) -> Active {
        self.conns.fetch_add(1, Ordering::SeqCst);
        Active(self.clone())
    }

    /// Count an attached MCP session until the guard drops.
    pub fn session(self: &Arc<Self>) -> Session {
        self.sessions.fetch_add(1, Ordering::SeqCst);
        Session(self.clone())
    }

    /// An attached MCP session or a live run keeps a replaced binary's daemon serving.
    pub fn busy(&self) -> bool {
        self.sessions.load(Ordering::SeqCst) > 0 || !lock(&self.runs).is_empty()
    }

    /// No connection and no live run for the idle time.
    pub fn idle(&self) -> bool {
        self.conns.load(Ordering::SeqCst) == 0 && lock(&self.runs).is_empty() && lock(&self.last).elapsed() >= self.idle
    }
}

/// The daemon's env minus `ITSE_*`, `CLAUDE_*` and `AI_AGENT`, which name its starter, not a server's.
pub fn base_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(k, _)| !(k.as_encoded_bytes().starts_with(b"ITSE_") || k.as_encoded_bytes().starts_with(b"CLAUDE_") || k == "AI_AGENT")).collect()
}

fn config(dirs: &Dirs) -> Config {
    fs::read(dirs.config.join("config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub struct Active(Arc<Host>);

impl Drop for Active {
    fn drop(&mut self) {
        *lock(&self.0.last) = Instant::now();
        self.0.conns.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Session(Arc<Host>);

impl Drop for Session {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Lsp,
    Dap,
    Mcp,
}

/// A resolved server: argv and env rendered, ready to spawn.
#[derive(Clone, Debug)]
pub struct Launch {
    pub id: String,
    pub protocol: Protocol,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    /// Exit code → mapped reason, such as 7 → `server_expired`.
    pub exit_codes: BTreeMap<i32, String>,
    pub settings: Value,
    pub init_options: Value,
    pub languages: BTreeMap<String, String>,
    pub idle_minutes: Option<u64>,
    pub timeout_s: Option<u64>,
    pub expose: bool,
    pub via: Option<DapVia>,
    pub hide_vars: Vec<String>,
    pub watch_client: bool,
}

/// rlimits set in `pre_exec` when declared (RLIMIT_CORE is always 0), and the OOM score written
/// after spawn: 800 for servers, 500 for runs, so restartable servers die before the agent does.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub nofile: Option<u64>,
    pub as_mb: Option<u64>,
    pub cpu_s: Option<u64>,
    #[serde(skip)]
    pub oom_score_adj: i32,
}

/// Spawn `cmd` as the leader of its own process group, or of its own session for a run's runner, so
/// a harness killing its tool's process tree misses it. `pre_exec` only sets rlimits (RLIMIT_CORE is
/// always 0) from a list built before the fork; the OOM score is written after the spawn.
pub fn spawn(cmd: &mut Command, limits: &Limits, session: bool, oom_score_adj: i32) -> io::Result<Child> {
    let set = [(sys::RLIMIT_CORE, Some(0)), (sys::RLIMIT_NOFILE, limits.nofile), (sys::RLIMIT_AS, limits.as_mb.map(|mb| mb << 20)), (sys::RLIMIT_CPU, limits.cpu_s)];
    let set: Vec<(i32, u64)> = set.into_iter().filter_map(|(r, v)| Some((r, v?))).collect();
    (!session).then(|| cmd.process_group(0));
    // SAFETY: setsid and setrlimit are async-signal-safe, and iterating the list allocates nothing.
    unsafe { cmd.pre_exec(move || if session { sys::new_session() } else { Ok(()) }.and_then(|()| set.iter().try_for_each(|&(r, v)| sys::set_rlimit(r, v)))) };
    let child = cmd.spawn()?;
    let _ = fs::write(format!("/proc/{}/oom_score_adj", child.id()), oom_score_adj.to_string());
    Ok(child)
}

/// The last `n` lines of a log, read from its last 64 KiB.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let bytes = fs::read(path).unwrap_or_default();
    let lines: Vec<String> = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(64 << 10)..]).lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Where a server is; `Starting` holds when, so `status` says `starting (41 s)`.
#[derive(Default)]
pub enum Phase {
    #[default]
    Stopped,
    Starting(Instant),
    /// A started server: its peer, and its process, the leader of its group.
    Up(Arc<Peer>, Arc<Mutex<Child>>),
    Crashed(Crash),
    Gone(&'static str),
}

/// What the reader thread keeps of a server (diagnostics, progress and registrations are not kept yet).
#[derive(Default)]
pub struct Live {
    pub last: VecDeque<Value>,
}

#[derive(Default)]
pub struct Slot {
    pub phase: Mutex<Phase>,
    pub started: Condvar,
    pub live: Arc<Mutex<Live>>,
}

impl Slot {
    /// The server's peer, started once however many callers ask at once: the first sets `Starting`
    /// and runs `start` with no lock held; the others wait for it and share its outcome. The note
    /// says why a start was needed after a crash or a daemon restart. A start that fails is
    /// `start_failed` and is not retried in the same call.
    pub fn up(&self, start: impl FnOnce() -> Result<(Arc<Peer>, Child)>) -> Result<(Arc<Peer>, Option<String>)> {
        let mut phase = lock(&self.phase);
        while let Phase::Starting(_) = *phase {
            phase = self.started.wait(phase).unwrap_or_else(PoisonError::into_inner);
            if let Phase::Crashed(c) = &*phase {
                bail!("start_failed: {}", c.text);
            }
        }
        if let Phase::Up(peer, _) = &*phase {
            return Ok((peer.clone(), None));
        }
        let note = match mem::replace(&mut *phase, Phase::Starting(Instant::now())) {
            Phase::Crashed(c) => Some(format!("restarted after crash ({})", c.text)),
            Phase::Gone(why) => Some(format!("started cold (gone: {why})")),
            _ => None,
        };
        drop(phase);
        // A start that panics ends `Crashed` too, so its waiters wake and the next call starts again.
        let started = panic::catch_unwind(AssertUnwindSafe(start)).unwrap_or_else(|_| Err(anyhow!("start panicked"))).map(|(peer, child)| (peer, Arc::new(Mutex::new(child))));
        *lock(&self.phase) = match &started {
            Ok((peer, child)) => Phase::Up(peer.clone(), child.clone()),
            Err(e) => Phase::Crashed(Crash { text: format!("{e:#}"), ..Crash::default() }),
        };
        self.started.notify_all();
        Ok((started.map_err(|e| e.context("start_failed"))?.0, note))
    }

    /// The phase in a few words, without waiting: a slot locked right now is `busy`.
    pub fn state(&self) -> String {
        match self.phase.try_lock().as_deref() {
            Ok(Phase::Stopped) => "stopped".into(),
            Ok(Phase::Starting(at)) => format!("starting ({} s)", at.elapsed().as_secs()),
            Ok(Phase::Up(_, child)) => format!("running (pid {})", lock(child).id()),
            Ok(Phase::Crashed(c)) => format!("crashed: {}", c.text),
            Ok(Phase::Gone(why)) => format!("gone: {why}"),
            Err(_) => "busy".into(),
        }
    }

    /// Stop the server: the slot is `Stopped` first, so its EOF isn't taken for a crash, then its
    /// group goes (LSP `shutdown`/`exit` and MCP's closed stdin will come first once servers are hosted). False if it
    /// wasn't up; any other phase stays, so a crash or a start under way isn't lost.
    pub fn stop(&self, grace: Duration) -> bool {
        let mut phase = lock(&self.phase);
        let Phase::Up(_, child) = &*phase else { return false };
        let child = child.clone();
        *phase = Phase::Stopped;
        drop(phase);
        let pid = lock(&child).id() as i32;
        sys::kill_group(pid, grace, || drop(lock(&child).try_wait()));
        let _ = lock(&child).wait();
        true
    }
}

/// How a server ended without being asked, in words (`exit 7, server_expired`, `killed by signal
/// 9 (likely out of memory)`), with what its log said last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Crash {
    pub text: String,
    /// From the package's `exit_codes`, such as `server_expired`.
    pub mapped: Option<String>,
    pub stderr_tail: Vec<String>,
}

impl Crash {
    pub fn new(status: ExitStatus, exit_codes: &BTreeMap<i32, String>, log: &Path) -> Self {
        let mapped = status.code().and_then(|c| exit_codes.get(&c).cloned());
        let text = match (status.code(), status.signal()) {
            (Some(code), _) => format!("exit {code}{}", mapped.as_ref().map_or(String::new(), |m| format!(", {m}"))),
            (_, Some(9)) => "killed by signal 9 (likely out of memory)".into(),
            (_, sig) => format!("killed by signal {}", sig.unwrap_or(0)),
        };
        Self { text, mapped, stderr_tail: tail(log, 5) }
    }
}

/// `next.json`: handle counters, never reused, so `run:14` names one run forever.
#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Next {
    pub run: u64,
    pub dbg: u64,
    pub q: u64,
}

impl Next {
    /// The next number of a handle kind (`run`, `dbg`, `q`) in `<ws state>/next.json`, as `kind:N`.
    pub fn take(state: &Path, kind: &str) -> anyhow::Result<String> {
        static HELD: Mutex<()> = Mutex::new(());
        let (_held, path) = (lock(&HELD), state.join("next.json"));
        let mut next: Next = fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let n = [("run", &mut next.run), ("dbg", &mut next.dbg)].into_iter().find(|(k, _)| *k == kind).map_or(&mut next.q, |(_, n)| n);
        *n += 1;
        let taken = format!("{kind}:{n}");
        sys::write_atomic(&path, &serde_json::to_vec(&next)?)?;
        Ok(taken)
    }
}

#[cfg(test)]
mod tests;
