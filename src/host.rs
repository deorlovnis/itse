//! The session host: the daemon's shared state (`Host`), its registry of
//! server slots and live runs, `procs.json`, how children are spawned (own process group, rlimits,
//! OOM score), what a crash leaves, and the clean-up after a daemon restart.
//!
//! A slot has two parts, so `status` never hangs on a start: `phase`, behind a Mutex and a Condvar,
//! which one caller moves to `Starting` and then starts the server with no lock held while the
//! others wait; and `live`, which only the server's reader thread fills, in short locks. Every lock
//! goes through `sys::lock`, so a panic that poisons one can't disable a server for good.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt, process::ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::{env, ffi::OsString, fs, fs::File, io, mem, panic, panic::AssertUnwindSafe, path::Path, path::PathBuf, thread, time::Duration, time::Instant, time::SystemTime};

use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::actions::{self, Run};
use crate::{lsp, manifest::Config, manifest::DapVia, rpc::Cancel, rpc::Peer, sys, sys::lock, usage::Usage, workspace::Dirs, workspace::Workspace};

/// The env var each server is started with, set to a value of its own, which its children inherit:
/// rust-analyzer starts `cargo check` in a session of its own, out of reach of the server's group.
pub const MARKER: &str = "ITSE_SERVER";
/// Bytes a server log may hold before it starts afresh.
pub const MAX_LOG: u64 = 4 << 20;

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
    /// `dbg:N` → its debug session, until it is stopped, reaped or found gone.
    pub debug: Mutex<BTreeMap<String, Arc<crate::dap::Session>>>,
    /// What `procs.json` holds: one record per live child, rewritten on every change.
    procs: Mutex<Vec<Value>>,
    /// The idle stops under way, each on a thread of its own: the daemon waits for them as it exits.
    stops: Mutex<Vec<thread::JoinHandle<()>>>,
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
        let (slots, runs, debug, procs, stops, started, idle) =
            (Mutex::default(), Mutex::default(), Mutex::default(), Mutex::default(), Mutex::default(), SystemTime::now(), Duration::from_secs(idle));
        Self { ws, dirs, socket, started, started_by, exe, base_env, idle, conns, sessions, last, stopping, bound, usage, slots, runs, debug, procs, stops }
    }

    /// A handle's slot, made `Stopped` on first use.
    pub fn slot(&self, handle: &str) -> Arc<Slot> {
        lock(&self.slots).entry(handle.into()).or_default().clone()
    }

    /// Add a child to `procs.json` (`Some`, replacing only a record of the same pid, so a server
    /// still being stopped keeps its own until its stop drops it), or drop `handle` from it; written
    /// atomically each time.
    pub fn track(&self, handle: &str, record: Option<Value>) {
        let pid = record.as_ref().map(|r| r["pid"].clone());
        self.retrack(|p| p["handle"] != handle || pid.as_ref().is_some_and(|pid| p["pid"] != *pid), record);
    }

    /// Drop the record of `handle`'s child `pid` only: a server started again meanwhile keeps its own.
    pub fn untrack(&self, handle: &str, pid: i32) {
        self.retrack(|p| p["handle"] != handle || p["pid"] != pid, None);
    }

    /// Spawn `argv` as `l` runs it (its cwd, its env on top of `env`, its limits), marked with
    /// `marker`, in its own group, stderr (and, unless `pipe`, stdout) appended to `log`; in
    /// `procs.json` as `handle` of `kind` from the spawn on. The log file is returned too.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_as(&self, handle: &str, kind: &str, l: &Launch, argv: &[String], env: &BTreeMap<String, String>, log: &Path, marker: &str, pipe: bool) -> Result<(Child, File)> {
        let (program, args) = argv.split_first().ok_or_else(|| anyhow!("{} has no command", l.id))?;
        let file = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log)?;
        let (input, output) = if pipe { (Stdio::piped(), Stdio::piped()) } else { (Stdio::null(), Stdio::from(file.try_clone()?)) };
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(&l.cwd).env_clear().envs(env).envs(l.env.iter().cloned()).env(MARKER, marker).stdin(input).stdout(output).stderr(file.try_clone()?);
        let child = spawn(&mut cmd, &l.limits, false, 800).map_err(|e| anyhow!("starting {program}: {e}"))?;
        let (pid, at) = (child.id() as i32, sys::utc(SystemTime::now()));
        let start_time = sys::proc_stat(pid).map_or(0, |s| s.start_time);
        let record = json!({"handle": handle, "kind": kind, "pid": pid, "pgid": pid, "start_time": start_time, "argv0": program, "started_at": at, "log": log, "marker": marker});
        self.track(handle, Some(record));
        Ok((child, file))
    }

    /// Kill `child`'s group and what carries `marker`, drop it from `procs.json`, and reap it.
    pub fn kill_child(&self, handle: &str, child: &mut Child, marker: &str) -> io::Result<ExitStatus> {
        let pid = child.id() as i32;
        kill_tree(pid, marker, || drop(child.try_wait()));
        self.untrack(handle, pid);
        child.wait()
    }

    fn retrack(&self, keep: impl Fn(&Value) -> bool, record: Option<Value>) {
        let mut procs = lock(&self.procs);
        procs.retain(keep);
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
        // The old records are rewritten only once what they name is killed, so a daemon that dies
        // meanwhile leaves them to the next one.
        for p in servers {
            let n = |k: &str| p[k].as_i64().unwrap_or_default();
            let (pid, pgid, start) = (n("pid") as i32, n("pgid") as i32, n("start_time") as u64);
            if sys::alive(pid, start) || (!sys::proc_stat(pid).is_some_and(|s| s.alive()) && pgid > 1 && sys::send_signal(-pgid, 0)) {
                sys::kill_group(pgid, Duration::from_secs(1), || ());
            }
            p["marker"].as_str().inspect(|m| kill_marked(m));
            *lock(&self.slot(p["handle"].as_str().unwrap_or_default()).phase) = Phase::Gone("daemon restarted");
        }
        self.track("", None);
        runs.into_iter().for_each(|p| actions::adopt(self, p));
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

    /// Count a connection until the guard drops; an open connection keeps the daemon alive.
    pub fn enter(self: &Arc<Self>) -> Count {
        self.counted(false)
    }

    /// Count an attached MCP session until the guard drops.
    pub fn session(self: &Arc<Self>) -> Count {
        self.counted(true)
    }

    fn counted(self: &Arc<Self>, session: bool) -> Count {
        self.count(session).fetch_add(1, Ordering::SeqCst);
        Count { host: self.clone(), session }
    }

    fn count(&self, session: bool) -> &AtomicUsize {
        if session { &self.sessions } else { &self.conns }
    }

    /// An attached MCP session, a live run or a live debug session keeps a replaced binary's daemon serving.
    pub fn busy(&self) -> bool {
        self.sessions.load(Ordering::SeqCst) > 0 || !lock(&self.runs).is_empty() || !lock(&self.debug).is_empty()
    }

    /// No connection, no live run or debug session and no server up, starting or stopping for the idle time.
    pub fn idle(&self) -> bool {
        let stopping = || lock(&self.stops).iter().any(|t| !t.is_finished());
        let serving = || stopping() || lock(&self.slots).values().any(|s| !matches!(s.phase.try_lock().as_deref(), Ok(Phase::Stopped | Phase::Crashed(_) | Phase::Gone(_))));
        self.conns.load(Ordering::SeqCst) == 0 && lock(&self.runs).is_empty() && lock(&self.debug).is_empty() && lock(&self.last).elapsed() >= self.idle && !serving()
    }

    /// Stop a server and drop it from `procs.json`; false if it wasn't up.
    pub fn stop_server(&self, handle: &str, wait: Duration) -> bool {
        let slot = lock(&self.slots).get(handle).cloned();
        slot.and_then(|s| s.stop(wait)).inspect(|&pid| self.untrack(handle, pid)).is_some()
    }

    /// A daemon tick: a server that ended unasked becomes `Crashed` and leaves `procs.json`; one
    /// that no query holds and that went unused for its idle time is taken out of its slot under the
    /// slot's lock, so no query gets it meanwhile, and stopped on a thread of its own, since a stop
    /// waits on the server. Nothing is restarted here: the next call that needs a server starts it.
    /// A server log past 4 MB starts afresh: a chatty server (or a `RA_LOG` passed down) fills no disk.
    /// Debug sessions are reaped too.
    pub fn reap(self: &Arc<Self>) {
        crate::dap::reap(self);
        let slots: Vec<(String, Arc<Slot>)> = lock(&self.slots).iter().map(|(h, s)| (h.clone(), s.clone())).collect();
        for (handle, slot) in slots {
            let Ok(mut phase) = slot.phase.try_lock() else { continue };
            if let Some(pid) = Slot::crashed(&mut phase) {
                drop(phase);
                self.untrack(&handle, pid);
                continue;
            }
            let Phase::Up(s) = &*phase else { continue };
            if fs::metadata(&s.log).is_ok_and(|m| m.len() > MAX_LOG) {
                // Its stderr is opened to append, so the server writes on at the new end.
                let _ = fs::OpenOptions::new().write(true).open(&s.log).and_then(|f| f.set_len(0));
            } else if !s.idle.is_zero() && Arc::strong_count(s) == 1 && lock(&slot.live).used.elapsed() >= s.idle {
                let (s, host) = (s.clone(), self.clone());
                *phase = Phase::Stopped;
                drop(phase);
                let stop = thread::spawn(move || host.untrack(&handle, s.stop(Duration::from_secs(5))));
                let mut stops = lock(&self.stops);
                stops.retain(|t| !t.is_finished());
                stops.push(stop);
            }
        }
    }

    /// Stop every server and end every debug session at once, as the daemon exits; each gets `wait`
    /// for its goodbye (a session's `disconnect` its own 3 s), and an idle stop under way gets to
    /// finish its own. A server still starting is in `procs.json` from its spawn on, and its group
    /// and marked processes are killed from there.
    pub fn stop_servers(self: &Arc<Self>, wait: Duration) {
        let handles: Vec<String> = lock(&self.slots).keys().cloned().collect();
        let mut stops: Vec<_> = handles.into_iter().map(|h| (self.clone(), h)).map(|(host, h)| thread::spawn(move || _ = host.stop_server(&h, wait))).collect();
        let debug: Vec<_> = lock(&self.debug).values().cloned().collect();
        stops.extend(debug.into_iter().map(|s| (self.clone(), s)).map(|(host, s)| thread::spawn(move || s.end(&host, "is gone: the daemon stopped"))));
        let idle: Vec<_> = mem::take(&mut *lock(&self.stops));
        stops.into_iter().chain(idle).for_each(|t| drop(t.join()));
        let left: Vec<Value> = lock(&self.procs).iter().filter(|p| p["kind"] != "run" && p["pgid"].is_i64()).cloned().collect();
        left.iter().for_each(|p| kill_tree(p["pgid"].as_i64().unwrap_or_default() as i32, p["marker"].as_str().unwrap_or_default(), || ()));
    }
}

/// The daemon's env minus `ITSE_*`, `CLAUDE_*` and `AI_AGENT`, which name its starter, not a server's.
pub fn base_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    vars.filter(|(k, _)| !(k.as_encoded_bytes().starts_with(b"ITSE_") || k.as_encoded_bytes().starts_with(b"CLAUDE_") || k == "AI_AGENT")).collect()
}

fn config(dirs: &Dirs) -> Config {
    fs::read(dirs.config.join("config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// A connection or an MCP session, counted while it lasts.
pub struct Count {
    host: Arc<Host>,
    session: bool,
}

impl Drop for Count {
    fn drop(&mut self) {
        // A connection restarts the idle clock as it ends; a session ends inside its connection.
        if !self.session {
            *lock(&self.host.last) = Instant::now();
        }
        self.host.count(self.session).fetch_sub(1, Ordering::SeqCst);
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
#[derive(Clone, Debug, Default)]
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
    /// The file extensions it serves, such as `rs`: the disk sync tells it about these only.
    pub filetypes: Vec<String>,
    /// What it writes into the workspace itself, as declared.
    pub outputs: Vec<String>,
    pub idle_minutes: Option<u64>,
    // The hosted MCP client reads these.
    #[allow(dead_code)]
    pub timeout_s: Option<u64>,
    #[allow(dead_code)]
    pub expose: bool,
    pub via: Option<DapVia>,
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

/// A server's group, then every process that carries its `marker` (what it started in sessions of
/// their own, such as rust-analyzer's `cargo check` and its build scripts), each with its group.
pub fn kill_tree(pgid: i32, marker: &str, reap: impl FnMut()) {
    sys::kill_group(pgid, Duration::from_secs(1), reap);
    kill_marked(marker);
}

fn kill_marked(marker: &str) {
    if marker.is_empty() {
        return;
    }
    let tag = format!("{MARKER}={marker}");
    let pids = fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok());
    let marked = pids.filter(|&pid| fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| env.split(|&b| b == 0).any(|v| v == tag.as_bytes())));
    let groups: std::collections::BTreeSet<i32> = marked.filter_map(|pid| sys::proc_stat(pid).map(|s| s.pgid)).collect();
    // Never the daemon's own group, whatever its env holds.
    let own = sys::proc_stat(std::process::id() as i32).map(|s| s.pgid);
    groups.into_iter().filter(|g| Some(*g) != own).for_each(|g| sys::kill_group(g, Duration::from_secs(1), || ()));
}

/// The last `n` lines of a log, read from its last 64 KiB.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let mut bytes = Vec::new();
    if let Ok(mut f) = fs::File::open(path) {
        let _ = f.seek(SeekFrom::End(-(64 << 10))).or_else(|_| f.seek(SeekFrom::Start(0))).and_then(|_| f.read_to_end(&mut bytes));
    }
    let lines: Vec<String> = String::from_utf8_lossy(&bytes).lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Where a server is; `Starting` holds when, so `status` says `starting (41 s)`.
#[derive(Default)]
pub enum Phase {
    #[default]
    Stopped,
    Starting(Instant),
    Up(Arc<Server>),
    Crashed(Crash),
    Gone(&'static str),
}

/// A started server: its peer, its process (the leader of its group), and what a crash, an idle
/// stop and `status` need to know about it.
pub struct Server {
    pub peer: Arc<Peer>,
    pub child: Mutex<Child>,
    pub exit_codes: BTreeMap<i32, String>,
    pub log: PathBuf,
    /// Unused this long, it is stopped; zero is never.
    pub idle: Duration,
    /// Of the launch it started from: another hash at the next resolve means its config changed.
    pub hash: String,
    /// Its `ITSE_SERVER` value.
    pub marker: String,
    /// Its start, as `Live::epoch`.
    pub epoch: u64,
}

/// What the reader thread keeps of a server, in short locks; `Slot::woke` tells `settle` it changed.
pub struct Live {
    /// The last log and show messages, for `status`.
    pub last: VecDeque<String>,
    pub lsp: lsp::Seen,
    /// When a call last used the server, for the idle stop.
    pub used: Instant,
    /// Counts the starts, so the reader of an earlier one records nothing here.
    pub epoch: u64,
}

impl Default for Live {
    fn default() -> Self {
        Self { last: VecDeque::new(), lsp: lsp::Seen::default(), used: Instant::now(), epoch: 0 }
    }
}

#[derive(Default)]
pub struct Slot {
    pub phase: Mutex<Phase>,
    pub started: Condvar,
    pub live: Arc<Mutex<Live>>,
    /// Notified by the reader after every message.
    pub woke: Arc<Condvar>,
    /// The documents queries have sent, held through a whole sync so two queries don't send one edit twice.
    pub docs: Mutex<lsp::Docs>,
}

impl Slot {
    /// The server, started once however many callers ask at once: the first sets `Starting`
    /// and runs `start` with no lock held; the others wait for it and share its outcome. The note
    /// says why a start was needed after a crash or a daemon restart; the flag, that this call
    /// started it or waited for its start, so its index is cold. A start that fails is
    /// `start_failed` and is not retried in the same call. A cancelled waiter stops waiting.
    pub fn up(&self, start: impl FnOnce() -> Result<Server>, cancel: &Cancel) -> Result<(Arc<Server>, Option<String>, bool)> {
        let (mut phase, mut waited) = (lock(&self.phase), false);
        while let Phase::Starting(_) = *phase {
            ensure!(!cancel.is_cancelled(), "cancelled");
            (phase, waited) = (self.started.wait_timeout(phase, Duration::from_millis(100)).unwrap_or_else(PoisonError::into_inner).0, true);
            if let Phase::Crashed(c) = &*phase {
                bail!("start_failed: {}", c.text);
            }
        }
        Self::crashed(&mut phase);
        if let Phase::Up(server) = &*phase {
            // Under the phase lock, so the idle stop can't take a server a query was just given.
            lock(&self.live).used = Instant::now();
            return Ok((server.clone(), None, waited));
        }
        let note = match mem::replace(&mut *phase, Phase::Starting(Instant::now())) {
            Phase::Crashed(c) => Some(format!("restarted after a crash ({})", c.text)),
            Phase::Gone(why) => Some(format!("started cold ({why})")),
            _ => None,
        };
        drop(phase);
        // A start that panics ends `Crashed` too, so its waiters wake and the next call starts again.
        let started = panic::catch_unwind(AssertUnwindSafe(start)).unwrap_or_else(|_| Err(anyhow!("start panicked"))).map(Arc::new);
        *lock(&self.phase) = match &started {
            Ok(server) => Phase::Up(server.clone()),
            Err(e) => Phase::Crashed(Crash { text: format!("{e:#}"), ..Crash::default() }),
        };
        self.started.notify_all();
        Ok((started.map_err(|e| e.context("start_failed"))?, note, true))
    }

    /// The server if it is up, without starting one; one that ended is made `Crashed` first, and
    /// its pid returned, for `procs.json`.
    pub fn get(&self) -> (Option<Arc<Server>>, Option<i32>) {
        let mut phase = lock(&self.phase);
        let ended = Self::crashed(&mut phase);
        let Phase::Up(server) = &*phase else { return (None, ended) };
        lock(&self.live).used = Instant::now();
        (Some(server.clone()), ended)
    }

    /// An `Up` server that exited, or whose stream closed, becomes `Crashed` with its exit and its
    /// log's tail; its pid if it did. Its group and marked processes are killed either way: they
    /// outlive their leader, so what the server started (a `cargo check`, a build script) would run
    /// on and hold its locks.
    pub fn crashed(phase: &mut Phase) -> Option<i32> {
        let Phase::Up(s) = &*phase else { return None };
        let mut child = lock(&s.child);
        let pid = child.id() as i32;
        child.try_wait().ok().filter(|ended| ended.is_some() || s.peer.closed().is_some())?;
        kill_tree(pid, &s.marker, || drop(child.try_wait()));
        let crash = Crash::new(child.wait().ok()?, &s.exit_codes, &s.log);
        drop(child);
        *phase = Phase::Crashed(crash);
        Some(pid)
    }

    /// The phase in a few words, without waiting: a slot locked right now is `busy`.
    pub fn state(&self) -> String {
        match self.phase.try_lock().as_deref() {
            Ok(Phase::Stopped) => "stopped".into(),
            Ok(Phase::Starting(at)) => format!("starting ({} s)", at.elapsed().as_secs()),
            Ok(Phase::Up(s)) => format!("up (pid {})", lock(&s.child).id()),
            Ok(Phase::Crashed(c)) => format!("crashed: {}", c.text),
            Ok(Phase::Gone(why)) => format!("gone: {why}"),
            Err(_) => "busy".into(),
        }
    }

    /// Stop the server: the slot is `Stopped` first, so its EOF isn't taken for a crash, then
    /// `Server::stop`. Its pid; None if it wasn't up, and any other phase stays, so a crash or a
    /// start under way isn't lost.
    pub fn stop(&self, wait: Duration) -> Option<i32> {
        let mut phase = lock(&self.phase);
        let Phase::Up(s) = &*phase else { return None };
        let s = s.clone();
        *phase = Phase::Stopped;
        drop(phase);
        Some(s.stop(wait))
    }
}

impl Server {
    /// The protocol's goodbye (LSP `shutdown`, waiting up to `wait`, and `exit`), up to 2 s for the
    /// process to go, and its group and marked processes are killed; its pid. Writes never block, so
    /// neither does this.
    pub fn stop(&self, wait: Duration) -> i32 {
        if self.peer.bye(wait) {
            sys::wait_until(wait.min(Duration::from_secs(2)), || !lock(&self.child).try_wait().is_ok_and(|st| st.is_none()));
        }
        let pid = lock(&self.child).id() as i32;
        kill_tree(pid, &self.marker, || drop(lock(&self.child).try_wait()));
        let _ = lock(&self.child).wait();
        pid
    }
}

/// How a server ended without being asked, in words (`exit 7, server_expired`, `killed by signal
/// 9 (out of memory, or killed)`), with what its log said last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Crash {
    pub text: String,
    /// The last lines of its log.
    pub stderr_tail: Vec<String>,
}

impl Crash {
    pub fn new(status: ExitStatus, exit_codes: &BTreeMap<i32, String>, log: &Path) -> Self {
        let mapped = status.code().and_then(|c| exit_codes.get(&c).cloned());
        let text = match (status.code(), status.signal()) {
            (Some(code), _) => format!("exit {code}{}", mapped.as_ref().map_or(String::new(), |m| format!(", {m}"))),
            (_, Some(9)) => "killed by signal 9 (out of memory, or killed)".into(),
            (_, sig) => format!("killed by signal {}", sig.unwrap_or(0)),
        };
        Self { text, stderr_tail: tail(log, 5) }
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
