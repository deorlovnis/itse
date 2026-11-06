//! The DAP client: a debug adapter spawned on stdio (`program`), spawned and then connected over
//! TCP (`tcp`), or reached at the port an LSP server's command hands out (`lsp_command`); the
//! handshake, the default stop rule of `debug test`, the events of a session, and the snapshot at a
//! stop that the `debug` verbs answer with.
//!
//! The core opens no network socket of its own, except that it may connect, never listen, to a
//! debug adapter's port on 127.0.0.1 or ::1, since adapters such as kotlin-lsp's hand out a TCP
//! port. The check is in code, before any socket exists. The adapter's own requests
//! (`runInTerminal`, `startDebugging`) are refused by the peer's default answer.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};
use std::{cell::Cell, fs, thread};

use anyhow::{Context, Result, anyhow, bail, ensure};
use regex_lite::Regex;
use serde_json::{Value, json};

use crate::host::{self, Host, Launch, Next, Protocol};
use crate::manifest::{DapVia, render_value};
use crate::resolve::{self, Resolved};
use crate::results::{Cost, Item, Outcome, State, Store, Verdict, clip};
use crate::rpc::{Cancel, Dialect, Framing, Handler, Peer, RpcError};
use crate::verbs::{Args, Caller};
use crate::{env, lsp, store, sys, sys::lock, usage::Event};

const INIT: Duration = Duration::from_secs(10);
const LAUNCH: Duration = Duration::from_secs(60);
const IDLE: Duration = Duration::from_secs(600);
/// A snapshot's budget: requests, the wait for each, and the wait for all.
const ASKS: usize = 50;
const ASK: Duration = Duration::from_secs(5);
const SNAP: Duration = Duration::from_secs(10);
/// Stops outside project code that a plain `caught` filter is continued past before one is shown.
const SKIPS: usize = 20;
/// How often a wait looks at its cancel and at the adapter.
const TICK: Duration = Duration::from_millis(100);
/// The snapshot text's cap, in chars.
const CAP: usize = 4000;
/// The usage log's class of a failed `debug` call, by what its error says; a `dbg:N` the caller
/// names that is not live is `unknown_id` where it is looked up, and the rest is `io`.
#[rustfmt::skip]
const CLASSES: [(&str, &str); 14] = [("cancelled", "cancel"), ("holds no", "unknown_id"), ("no stored result", "unknown_id"), ("no answer to", "timeout"),
    ("no initialized event", "timeout"), ("no extension in use declares", "not_installed"), (" takes ", "bad_args"), ("missing argument", "bad_args"), (": no such file", "bad_args"),
    (" is not path:line", "bad_args"), (" is not a port", "bad_args"), (" is not 127.0.0.1", "bad_args"), ("project frames, so no", "bad_args"), ("has not stopped yet", "bad_args")];
/// What a session that failed to start leaves.
const FAILED: &str = "is not running: its start failed";

/// Connect to `host:port` if `host` is the literal 127.0.0.1 or ::1, trying `tries` times 100 ms
/// apart, since an adapter just spawned may not listen yet. Names are refused too: a name could
/// resolve anywhere, and resolving one is a socket of its own.
pub fn connect_loopback(host: &str, port: u16, tries: u32) -> Result<TcpStream> {
    let ip = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok();
    let Some(ip) = ip.filter(|ip| *ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || *ip == IpAddr::V6(Ipv6Addr::LOCALHOST)) else {
        bail!("{host} is not 127.0.0.1 or ::1; itse connects to debug adapters on loopback only");
    };
    let connect = || TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_millis(100));
    let early = (1..tries).find_map(|_| connect().inspect_err(|_| thread::sleep(Duration::from_millis(100))).ok());
    early.map_or_else(connect, Ok).with_context(|| format!("connecting to {host} port {port}"))
}

/// Where an LSP command's debug server listens, or an `attach` address: `5005`, `"5005"`,
/// `{"port": 5005}` (with `host`, if given) or `"host:port"`; the host defaults to 127.0.0.1.
fn address(v: &Value) -> Result<(String, u16)> {
    let text = match v {
        Value::Object(o) => format!("{}:{}", o.get("host").and_then(Value::as_str).unwrap_or("127.0.0.1"), o.get("port").map_or(String::new(), Value::to_string)),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let (host, port) = text.rsplit_once(':').unwrap_or(("127.0.0.1", &text));
    Ok((host.into(), port.trim_matches('"').parse().map_err(|_| anyhow!("{v} is not a port or host:port"))?))
}

type Queue = (VecDeque<(String, Value)>, VecDeque<String>);

/// What the adapter sends unasked: the events a session waits on, queued for its calls, and the
/// last 50 lines of `output` (what logpoints and the debuggee print; not `telemetry`, which is no
/// one's to read). Other events are dropped, so a chatty adapter grows nothing.
#[derive(Default)]
struct Events {
    q: Mutex<Queue>,
    woke: Condvar,
}

impl Handler for Events {
    fn notify(&self, event: &str, body: Value) {
        let mut q = lock(&self.q);
        match event {
            "output" if body["category"] != "telemetry" => q.1.extend(body["output"].as_str().unwrap_or("").lines().map(|l| clip(l, 160))),
            "initialized" | "stopped" | "continued" | "exited" | "terminated" => q.0.push_back((event.into(), body)),
            _ => return,
        }
        let over = q.1.len().saturating_sub(50);
        q.1.drain(..over);
        self.woke.notify_all();
    }
}

/// Where a session stops first: line breakpoints and logpoints by source, or caught exceptions in
/// project code (by these class or module prefixes); the rule in words, for the first line, and
/// where the logpoints are.
#[derive(Debug, Default)]
struct Rule {
    lines: BTreeMap<PathBuf, Vec<Value>>,
    caught: Option<Vec<String>>,
    text: String,
    logs: Vec<String>,
}

impl Rule {
    /// `setExceptionBreakpoints` for this rule and adapter, and whether its caught filter is the
    /// plain one, whose stops outside the project are skipped. Nothing for line breakpoints; else
    /// `uncaught`, and `caught` scoped by the extension's `condition` (in the adapter's own syntax,
    /// `${packages}` filled) where the adapter takes one. `exceptionOptions` is not sent, even where
    /// offered: its paths name exception types, not the code that throws, so it can't scope to the
    /// project (an open point for the JVM adapters).
    fn exceptions(&self, caps: &Value, condition: Option<&str>) -> (Value, bool) {
        let Some(pre) = &self.caught else { return (json!({"filters": []}), false) };
        let filters = caps["exceptionBreakpointFilters"].as_array().cloned().unwrap_or_default();
        let find = |ids: &[&str]| filters.iter().find(|f| f["filter"].as_str().is_some_and(|id| ids.contains(&id)));
        let (caught, uncaught) = (find(&["caught", "raised", "all"]), find(&["uncaught"]));
        let ids = |fs: &[Option<&Value>]| fs.iter().flatten().map(|f| f["filter"].clone()).collect::<Vec<_>>();
        match (caught, condition) {
            (Some(c), Some(cond)) if !pre.is_empty() && caps["supportsExceptionFilterOptions"] == true && c["supportsCondition"] == true => {
                (json!({"filters": ids(&[uncaught]), "filterOptions": [{"filterId": c["filter"], "condition": cond.replace("${packages}", &pre.join(","))}]}), false)
            }
            _ => (json!({"filters": ids(&[caught, uncaught])}), caught.is_some()),
        }
    }
}

/// An argument that names a stored item, `run:14/T1`: the result and the item.
fn named(arg: &str) -> Option<(&str, &str)> {
    arg.split_once('/').filter(|(id, _)| id.contains(':'))
}

/// `--at path:line` breakpoints and `--log path:line=text` logpoints, which never stop (so a
/// logpoint needs its text: an empty `logMessage` is a breakpoint); a path is the caller's folder's,
/// else the root's, as every path the CLI takes, and must be a file. Without an `--at`: a line
/// breakpoint at the frame of the T item named (`run:14/T1`), else of the first failing test in the
/// newest stored result whose scope starts with `tests`, in place of a logpoint there; else caught
/// exceptions in `packages`, else in the prefixes that result's test names give, else in project code.
fn rule(root: &Path, cwd: &Path, store: &Store, args: &Args, tests: Option<&str>, packages: &[String]) -> Result<Rule> {
    let (mut r, mut at, flag) = (Rule::default(), Vec::new(), |k: &str| args.flags.get(k).and_then(Value::as_array).cloned().unwrap_or_default());
    let (ats, logs) = (flag("at"), flag("log"));
    let logs = logs.iter().filter_map(Value::as_str).map(|l| l.split_once('=').filter(|(_, t)| !t.is_empty()).ok_or_else(|| anyhow!("'{l}' is not path:line=text")));
    for spec in ats.iter().filter_map(Value::as_str).map(|a| Ok((a, ""))).chain(logs) {
        let (spec, log) = spec?;
        let (path, line) = spec.rsplit_once(':').and_then(|(p, n)| Some((p, n.parse::<u64>().ok()?))).ok_or_else(|| anyhow!("'{spec}' is not path:line"))?;
        let file = [cwd.join(path), root.join(path)].into_iter().find(|p| p.is_file()).ok_or_else(|| anyhow!("{path}: no such file"))?;
        let spot = format!("{}:{line}", file.strip_prefix(root).unwrap_or(&file).display());
        let bp = if log.is_empty() { json!({"line": line}) } else { json!({"line": line, "logMessage": log}) };
        if log.is_empty() { &mut at } else { &mut r.logs }.push(spot);
        r.lines.entry(file).or_default().push(bp);
    }
    if !at.is_empty() {
        r.text = format!("breakpoints at {}", at.join(", "));
        return Ok(r);
    }
    let (name, newest) = (args.rest.iter().find_map(|a| named(a)), tests.and_then(|t| newest(store, t)));
    let failed = match name {
        Some((id, item)) => Some(store.load(id)?.0.items.into_iter().find(|i| i.id == item).ok_or_else(|| anyhow!("{id} holds no {item}"))?),
        None => newest.iter().flat_map(|o| &o.items).find(|i| i.kind == "failed").cloned(),
    };
    if let Some((it, loc)) = failed.as_ref().and_then(|i| Some((i, i.loc.as_ref()?))) {
        let (spot, bps) = (format!("{}:{}", loc.path, loc.line), r.lines.entry(root.join(&loc.path)).or_default());
        bps.retain(|b| b["line"] != loc.line);
        bps.push(json!({"line": loc.line}));
        let replaced = if r.logs.contains(&spot) { " (in place of the logpoint there)" } else { "" };
        r.logs.retain(|l| *l != spot);
        r.text = format!("breakpoint at {}'s frame {spot}{replaced}", name.map_or(it.id.clone(), |(id, item)| format!("{id}/{item}")));
        return Ok(r);
    }
    let caught = if packages.is_empty() { newest.map_or(vec![], |o| prefixes(&o.items)) } else { packages.to_vec() };
    r.text = format!("caught exceptions in {}", if caught.is_empty() { "project code".into() } else { caught.join(", ") });
    r.caught = Some(caught);
    Ok(r)
}

/// The newest stored test result whose scope starts with `tests`.
fn newest(store: &Store, tests: &str) -> Option<Outcome> {
    let mut runs: Vec<u64> = fs::read_dir(store.0.join("runs")).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
    runs.sort_unstable();
    runs.iter().rev().filter_map(|n| Some(store.load(&format!("run:{n}")).ok()?.0)).find(|o| o.verb == "test" && o.scope.starts_with(tests))
}

/// The class or module prefixes test names give, as frames name the code: `orders::tests::saves`
/// gives `orders` and `com.acme.OrderTest.saves` gives `com.acme`; a shorter name, none.
fn prefixes(items: &[Item]) -> Vec<String> {
    let names = items.iter().filter_map(|i| i.msg.split_whitespace().next());
    let parts = names.map(|n| if n.contains("::") { ("::", n.split("::").collect::<Vec<_>>()) } else { (".", n.split('.').collect()) });
    parts.filter(|(_, p)| p.len() > 2).map(|(sep, p)| p[..p.len() - 2].join(sep)).collect::<BTreeSet<_>>().into_iter().collect()
}

/// A frame's source as a repo path, if it is the project's: a path or `file://` URI (decoded) under
/// the root, relative to it, or in another checkout or mount of it (what follows the root's folder
/// name), that is a file there, without `..` and not under `build/` or `target/`.
fn project(root: &Path, f: &Value) -> Option<String> {
    let raw = f["source"]["path"].as_str()?;
    let path = lsp::path_of(raw).unwrap_or_else(|| raw.into());
    let rel: PathBuf = match path.strip_prefix(root) {
        Ok(rel) => rel.into(),
        Err(_) if path.is_relative() => path.clone(),
        Err(_) => path.components().skip_while(|c| Some(c.as_os_str()) != root.file_name()).skip(1).collect(),
    };
    let plain = rel.components().all(|c| matches!(c, Component::Normal(n) if n != "build" && n != "target"));
    (plain && root.join(&rel).is_file()).then(|| rel.display().to_string())
}

/// Whether something listens on `port` already, as /proc/net/tcp and tcp6 say (state 0A).
fn listening(port: u16) -> bool {
    let (at, tables) = (format!(":{port:04X}"), ["/proc/net/tcp", "/proc/net/tcp6"].map(|p| fs::read_to_string(p).unwrap_or_default()));
    tables.iter().flat_map(|t| t.lines()).map(|l| l.split_whitespace().collect::<Vec<_>>()).any(|c| c.len() > 3 && c[1].ends_with(&at) && c[3] == "0A")
}

/// The marker an adapter's env passes down to what it starts (a debuggee in a session of its own),
/// so the session's end, or the next daemon, kills that too.
fn marker(handle: &str) -> String {
    format!("{}-{handle}", std::process::id())
}

/// What a session holds besides its stream: the adapter's process when it spawned one, its socket
/// over TCP, and the LSP server that serves it, held so that server's idle stop leaves it be.
#[derive(Default)]
struct Ends {
    child: Option<Mutex<Child>>,
    tcp: Option<TcpStream>,
    _server: Option<Arc<host::Server>>,
    /// What the LSP server's ask noted (a config changed since it started).
    notes: Vec<String>,
}

type Stream = (Box<dyn BufRead + Send>, Box<dyn Write + Send>, Ends);

fn tcp(s: TcpStream, ends: Ends) -> Result<Stream> {
    Ok((Box::new(BufReader::new(s.try_clone()?)), Box::new(s.try_clone()?), Ends { tcp: Some(s), ..ends }))
}

/// Spawn an adapter in its own group, marked, in `procs.json` as the session's handle, stderr
/// (and, off stdio, stdout) appended to `log`.
fn spawn(host: &Host, l: &Launch, argv: &[String], handle: &str, log: &Path, env: &BTreeMap<String, String>, stdio: bool) -> Result<Child> {
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("{} has no command", l.id))?;
    let file = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log)?;
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(&l.cwd).env_clear().envs(env).envs(l.env.iter().cloned()).env(host::MARKER, marker(handle)).stderr(file.try_clone()?);
    let (input, output) = if stdio { (Stdio::piped(), Stdio::piped()) } else { (Stdio::null(), Stdio::from(file)) };
    cmd.stdin(input).stdout(output);
    let child = host::spawn(&mut cmd, &l.limits, false, 800).map_err(|e| anyhow!("starting {program}: {e}"))?;
    let (pid, at) = (child.id() as i32, sys::utc(SystemTime::now()));
    let start_time = sys::proc_stat(pid).map_or(0, |s| s.start_time);
    let record =
        json!({"handle": handle, "kind": "dap", "pid": pid, "pgid": pid, "start_time": start_time, "argv0": program, "started_at": at, "log": log, "marker": marker(handle)});
    host.track(handle, Some(record));
    Ok(child)
}

/// Reach the adapter as its `via` says: spawned on stdio; spawned, then connected to its declared
/// port (free before the spawn, else this would reach whatever holds it); or connected to the port
/// its LSP server's command answers with. Loopback only.
#[allow(clippy::too_many_arguments)]
fn open(host: &Arc<Host>, r: &Resolved, caller: &Caller, l: &Launch, handle: &str, log: &Path, env: &BTreeMap<String, String>, cancel: &Cancel) -> Result<Stream> {
    match l.via.as_ref().unwrap_or(&DapVia::Program) {
        DapVia::Program => {
            let mut child = spawn(host, l, &l.argv, handle, log, env, true)?;
            let (Some(out), Some(input)) = (child.stdout.take(), child.stdin.take()) else { bail!("{} has no stdio", l.id) };
            Ok((Box::new(BufReader::new(out)), Box::new(input), Ends { child: Some(Mutex::new(child)), ..Ends::default() }))
        }
        DapVia::Tcp { argv, port } => {
            ensure!(!listening(*port), "port {port} is taken already (by another debug session?), so {} can't listen there", l.id);
            let mut child = spawn(host, l, argv, handle, log, env, false)?;
            let pid = child.id() as i32;
            let connected = connect_loopback("127.0.0.1", *port, 50).inspect_err(|_| host::kill_tree(pid, &marker(handle), || drop(child.try_wait())));
            tcp(connected.inspect_err(|_| host.untrack(handle, pid))?, Ends { child: Some(Mutex::new(child)), ..Ends::default() })
        }
        DapVia::LspCommand { server, command, arguments } => {
            let filetypes = r.launches.iter().find(|x| x.protocol == Protocol::Lsp && x.id == *server).map(|x| x.filetypes.clone());
            let filetypes = filetypes.ok_or_else(|| anyhow!("lsp:{server} is not ready; itse status shows why"))?;
            let asks = lsp::servers(host, &caller.env, "debug", "", &|f| f == filetypes.as_slice(), true, cancel).map_err(|o| anyhow!("{}", o.head()))?;
            let ask = asks.asks.into_iter().find(|a| a.launch.id == *server).ok_or_else(|| anyhow!("lsp:{server} is not running"))?;
            let answer = ask.request("workspace/executeCommand", json!({"command": command, "arguments": arguments}), cancel)?;
            let s = address(&answer).and_then(|(h, port)| connect_loopback(&h, port, 50)).with_context(|| format!("lsp:{server}'s {command} answered {answer}"))?;
            tcp(s, Ends { _server: ask._held.clone(), notes: ask.notes.iter().filter(|n| n.contains("config changed")).cloned().collect(), ..Ends::default() })
        }
    }
}

/// One debug session, `dbg:N`.
pub struct Session {
    handle: String,
    peer: Arc<Peer>,
    events: Arc<Events>,
    caps: OnceLock<Value>,
    ends: Ends,
    /// A launched debuggee ends with the session; an attached one goes on.
    launch: bool,
    root: PathBuf,
    state: PathBuf,
    hide: Vec<Regex>,
    /// The stopped thread, the last snapshot, the snapshots taken, the last use.
    at: Mutex<(i64, Value, u32, Instant)>,
    /// A stop not come by the call's wait, which `debug snapshot` waits for: Some(plain), whether a
    /// plain caught filter's framework stops are skipped on the way.
    pending: Mutex<Option<bool>>,
    ended: AtomicBool,
    /// What started it, for status, and the adapter's log, kept under 4 MiB.
    scope: String,
    log: PathBuf,
}

/// What a wait for a stop came to: the `stopped` body, how the debuggee ended, or nothing yet.
enum Stop {
    At(Value),
    Over(String),
    Pending,
}

/// `cap`, or what is left until `until` if less.
fn left(until: Instant, cap: Duration) -> Duration {
    cap.min(until.saturating_duration_since(Instant::now()))
}

impl Session {
    /// A request; an adapter that ended makes the session gone.
    fn request(&self, cmd: &str, args: Value, wait: Duration, cancel: &Cancel) -> Result<Value> {
        self.peer.request(cmd, args, wait, cancel).map_err(|e| self.failed(cmd, e))
    }

    fn failed(&self, cmd: &str, e: RpcError) -> anyhow::Error {
        match e {
            RpcError::Closed(_) => anyhow!("{}", gone(&self.state, &self.handle, &self.lost().unwrap_or_default())),
            e => anyhow!("{cmd}: {e}"),
        }
    }

    /// A request whose refusal is a note, not the session's end.
    fn soft(&self, cmd: &str, args: Value, cancel: &Cancel) -> Result<Result<Value, String>> {
        match self.request(cmd, args, ASK, cancel) {
            Err(e) if self.peer.closed().is_none() && !cancel.is_cancelled() => Ok(Err(format!("{e:#}"))),
            r => r.map(Ok),
        }
    }

    /// What became of the session if its adapter ended.
    fn lost(&self) -> Option<String> {
        self.peer.closed().map(|why| format!("is gone: the adapter ended ({why})"))
    }

    /// An error once the call is cancelled or the adapter has ended.
    fn alive(&self, cancel: &Cancel) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "cancelled");
        self.lost().map_or(Ok(()), |why| bail!("{}", gone(&self.state, &self.handle, &why)))
    }

    fn can(&self, cap: &str) -> bool {
        self.caps.get().is_some_and(|c| c[cap] == true)
    }

    /// `initialize`, then `launch` or `attach` sent without waiting (java-debug-style adapters
    /// answer it only after `configurationDone`), the `initialized` event (or the refusal of the
    /// launch, which some adapters send instead), the rule's breakpoints (logpoints only where
    /// supported) and exception filters, `configurationDone`, and the answer to `launch` or
    /// `attach`. Whether the caught filter is plain, and what went amiss on the way: a refused
    /// request, a source that isn't a file, a breakpoint left unverified, logpoints dropped.
    /// A caught condition the adapter refuses falls back to the plain filter. Each wait ends by `until`.
    #[allow(clippy::too_many_arguments)]
    fn handshake(&self, adapter: &str, request: &str, arguments: Value, rule: &Rule, condition: Option<&str>, until: Instant, cancel: &Cancel) -> Result<(bool, Vec<String>)> {
        let init = json!({"clientID": "itse", "clientName": "Itse", "adapterID": adapter, "pathFormat": "path", "linesStartAt1": true, "columnsStartAt1": true,
            "supportsVariableType": true, "supportsRunInTerminalRequest": false, "supportsStartDebuggingRequest": false, "locale": "en"});
        let caps = self.request("initialize", init, left(until, INIT), cancel)?;
        self.peer.dap_cancel.store(caps["supportsCancelRequest"] == true, Ordering::SeqCst);
        let _ = self.caps.set(caps.clone());
        let answer = self.peer.send_request(request, arguments);
        let (mut early, wait, mut notes) = (None, left(until, INIT), Vec::new());
        let end = Instant::now() + wait;
        while self.next(&["initialized"], TICK, cancel).is_none() {
            early = early.or_else(|| answer.try_recv().ok());
            if matches!(early, Some(Err(_))) {
                break;
            }
            self.alive(cancel)?;
            ensure!(Instant::now() < end, "no initialized event in {} s", wait.as_secs());
        }
        let (exceptions, mut plain) = rule.exceptions(&caps, condition);
        if !matches!(early, Some(Err(_))) {
            let logs = caps["supportsLogPoints"] == true;
            for (path, bps) in &rule.lines {
                let rel = path.strip_prefix(&self.root).unwrap_or(path).display();
                let dropped = !logs && bps.iter().any(|b| b.get("logMessage").is_some());
                let missing = (!path.is_file()).then(|| format!("{rel}: no such file"));
                notes.extend(missing.into_iter().chain(dropped.then(|| format!("{rel}: logpoints dropped, as the adapter has none"))));
                let bps: Vec<&Value> = bps.iter().filter(|b| logs || b.get("logMessage").is_none()).collect();
                let set = self.soft("setBreakpoints", json!({"source": {"path": path}, "breakpoints": bps}), cancel)?.map_err(|e| notes.push(e)).unwrap_or_default();
                let unverified = set["breakpoints"].as_array().into_iter().flatten().zip(&bps).filter(|(b, _)| b["verified"] == false);
                notes.extend(unverified.map(|(b, bp)| format!("{rel}:{} unverified{}", bp["line"], b["message"].as_str().map_or(String::new(), |m| format!(" ({m})")))));
            }
            match self.soft("setExceptionBreakpoints", exceptions.clone(), cancel)? {
                Err(e) if exceptions.get("filterOptions").is_some() => {
                    let (exceptions, p) = rule.exceptions(&caps, None);
                    (plain, _) = (p, notes.push(format!("{e}, so the plain caught filter, its stops outside the project skipped")));
                    notes.extend(self.soft("setExceptionBreakpoints", exceptions, cancel)?.err());
                }
                set => notes.extend(set.err()),
            }
            if caps["supportsConfigurationDoneRequest"] == true {
                self.request("configurationDone", json!({}), ASK, cancel)?;
            }
        }
        let wait = left(until, LAUNCH);
        let end = Instant::now() + wait;
        let answered = loop {
            if let Some(a) = early.take().or_else(|| answer.recv_timeout(TICK).ok()) {
                break a;
            }
            self.alive(cancel)?;
            ensure!(Instant::now() < end, "no answer to {request} in {} s", wait.as_secs());
        };
        answered.map(|_| (plain, notes)).map_err(|e| self.failed(request, e))
    }

    /// The first event named in `names`, those queued before it dropped; None after `wait`, a
    /// cancel or the adapter's end.
    fn next(&self, names: &[&str], wait: Duration, cancel: &Cancel) -> Option<(String, Value)> {
        let (end, mut q) = (Instant::now() + wait, lock(&self.events.q));
        loop {
            if let Some(i) = q.0.iter().position(|(e, _)| names.contains(&e.as_str())) {
                return q.0.drain(..=i).next_back();
            }
            if self.peer.closed().is_some() || cancel.is_cancelled() || Instant::now() >= end {
                return None;
            }
            q = self.events.woke.wait_timeout(q, TICK).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Send a step or a continue, the stops and continues queued before it dropped: a stale stop
    /// (another thread's, say) never answers it.
    fn resume(&self, cmd: &str, args: Value, cancel: &Cancel) -> Result<Value> {
        lock(&self.events.q).0.retain(|(e, _)| e != "stopped" && e != "continued");
        self.request(cmd, args, ASK, cancel)
    }

    /// The next stop by `until`, else how the debuggee ended, in words; a step of `thread` skips
    /// another thread's own stop. The `threadId` is optional (all threads stopped): the first
    /// thread's then.
    fn wait_stop(&self, until: Instant, thread: Option<i64>, cancel: &Cancel) -> Result<Stop> {
        loop {
            let Some((event, mut body)) = self.next(&["stopped", "terminated", "exited"], left(until, IDLE), cancel) else {
                self.alive(cancel)?;
                return Ok(Stop::Pending);
            };
            if event != "stopped" {
                let output: Vec<String> = lock(&self.events.q).1.iter().rev().take(3).rev().cloned().collect();
                let code = body["exitCode"].as_i64().map_or(String::new(), |c| format!(" with exit code {c}"));
                return Ok(Stop::Over(format!("the debuggee ended{code}{}", if output.is_empty() { String::new() } else { format!("; output: {}", output.join(" | ")) })));
            }
            if thread.is_some_and(|t| body["threadId"].as_i64().is_some_and(|b| b != t)) && body["allThreadsStopped"] != true {
                continue;
            }
            if body["threadId"].is_null() {
                body["threadId"] = self.request("threads", json!({}), ASK, cancel)?["threads"][0]["id"].clone();
            }
            return Ok(Stop::At(body));
        }
    }

    /// Run to the first stop the rule keeps: on a plain caught filter, a caught exception (as
    /// `exceptionInfo`'s break mode tells, where offered) whose top frame isn't the project's is
    /// continued, 20 times at most; a stop these requests fail on is kept. The stop, how the
    /// debuggee ended, or nothing by `until`; and how many stops were skipped.
    fn first_stop(&self, plain: bool, until: Instant, cancel: &Cancel) -> Result<(Stop, usize)> {
        let mut skipped = 0;
        loop {
            let stop = match self.wait_stop(until, None, cancel)? {
                Stop::At(stop) => stop,
                other => return Ok((other, skipped)),
            };
            let (thread, ask) = (&stop["threadId"], |cmd: &str, args: Value| self.request(cmd, args, ASK, cancel));
            let uncaught = || self.can("supportsExceptionInfoRequest") && ask("exceptionInfo", json!({"threadId": thread})).is_ok_and(|i| i["breakMode"] != "always");
            let ours = || ask("stackTrace", json!({"threadId": thread, "levels": 1})).map_or(true, |t| project(&self.root, &t["stackFrames"][0]).is_some());
            if !plain || stop["reason"] != "exception" || skipped >= SKIPS || uncaught() || ours() {
                return Ok((Stop::At(stop), skipped));
            }
            self.resume("continue", json!({"threadId": thread}), cancel)?;
            skipped += 1;
        }
    }

    /// What a wait for a stop answers: the snapshot at it; EXITED, the session ended; or RUNNING,
    /// kept for `debug snapshot` to wait on (with `plain` for its skips). `rule` is a start's.
    #[allow(clippy::too_many_arguments)]
    fn settle(&self, host: &Host, caller: &Caller, scope: &str, stop: Stop, plain: bool, rule: Option<&str>, frame: usize, cancel: &Cancel) -> Result<Outcome> {
        *lock(&self.pending) = matches!(stop, Stop::Pending).then_some(plain);
        let cost = Cost { ms: lock(&self.at).3.elapsed().as_millis() as u64, warm: None };
        let on = rule.map_or(String::new(), |r| format!(" (stop on: {r})"));
        let counts = match stop {
            Stop::At(stop) => return self.answer(caller, scope, &stop, rule, frame, cancel),
            Stop::Over(over) => (Verdict::Exited, if rule.is_some() { format!("no stop{on}; {over}") } else { over }),
            Stop::Pending => (Verdict::Running, format!("no stop yet{on}; 'itse debug snapshot {}' waits for it", self.handle)),
        };
        let id = (counts.0 == Verdict::Running).then(|| self.handle.clone());
        if counts.0 == Verdict::Exited {
            self.end(host, "has ended");
        }
        Ok(Outcome { cost, id, ..Outcome::new("debug", counts.0, scope, counts.1) })
    }

    /// The stopped thread at most 50 requests deep (5 s each, 10 s in all): its project frames
    /// (the rest counted), their source windows, the variables of the top three, the exception and
    /// the last output; `cut` when the budget ran out or a request failed. Names are clipped too.
    fn snapshot(&self, thread: i64, reason: &str, cancel: &Cancel) -> Result<Value> {
        let (left, end, lost) = (Cell::new(ASKS), Instant::now() + SNAP, Cell::new(false));
        let ask = |cmd: &str, args: Value| -> Result<Value> {
            let wait = end.saturating_duration_since(Instant::now()).min(ASK);
            ensure!(left.get() > 0 && !wait.is_zero(), "the snapshot's budget is spent");
            left.set(left.get() - 1);
            self.request(cmd, args, wait, cancel).inspect_err(|_| lost.set(true))
        };
        let threads = ask("threads", json!({}))?;
        let name = threads["threads"].as_array().into_iter().flatten().find(|t| t["id"] == thread).map_or("", |t| t["name"].as_str().unwrap_or(""));
        let all = ask("stackTrace", json!({"threadId": thread, "startFrame": 0, "levels": 40}))?["stackFrames"].as_array().cloned().unwrap_or_default();
        let mut frames = Vec::new();
        for f in &all {
            let Some(path) = project(&self.root, f) else { continue };
            let line = f["line"].as_u64().unwrap_or(1);
            let text = fs::read_to_string(self.root.join(&path)).unwrap_or_default();
            let src: Vec<(u64, String)> = text.lines().zip(1..).filter(|(_, n)| n + 2 >= line && *n <= line + 2).map(|(l, n)| (n, clip(l, 160))).collect();
            let mut frame = json!({"id": f["id"], "name": clip(f["name"].as_str().unwrap_or("?"), 160), "path": path, "line": line, "col": f["column"], "src": src});
            if frames.len() < 3 {
                let scopes = ask("scopes", json!({"frameId": f["id"]})).map(|s| s["scopes"].clone()).unwrap_or_default();
                let cheap = scopes.as_array().into_iter().flatten().filter(|s| s["expensive"] != true);
                frame["scopes"] = cheap.map(|s| json!({"name": s["name"], "vars": self.vars(&ask, &s["variablesReference"], 20, true)})).collect();
            }
            frames.push(frame);
        }
        let exception = (reason == "exception" && self.can("supportsExceptionInfoRequest")).then(|| ask("exceptionInfo", json!({"threadId": thread})).ok());
        let output: Vec<String> = lock(&self.events.q).1.iter().cloned().collect();
        let cut = lost.get() || left.get() == 0 || Instant::now() >= end;
        let thread = json!({"id": thread, "name": clip(name, 160)});
        Ok(json!({"reason": reason, "thread": thread, "frames": frames, "framework": all.len() - frames.len(), "exception": exception.flatten(), "output": output, "cut": cut}))
    }

    /// The variables under `reference`, at most `max`, those `hide_vars` names dropped, values
    /// clipped to 160 chars and names and types to 80; with `deep`, `this`, `self` and an exception
    /// get theirs too (10 at most).
    fn vars(&self, ask: &dyn Fn(&str, Value) -> Result<Value>, reference: &Value, max: usize, deep: bool) -> Vec<Value> {
        let Some(got) = reference.as_i64().filter(|r| *r > 0).and_then(|r| ask("variables", json!({"variablesReference": r})).ok()) else { return vec![] };
        let shown = got["variables"].as_array().into_iter().flatten().filter(|v| !self.hide.iter().any(|h| h.is_match(v["name"].as_str().unwrap_or(""))));
        let one = |v: &Value| {
            let s = |k: &str, max: usize| clip(v[k].as_str().unwrap_or(""), max);
            let name = v["name"].as_str().unwrap_or("");
            let more = deep && (matches!(name, "this" | "self") || name.to_lowercase().contains("exception"));
            let children = if more { self.vars(ask, &v["variablesReference"], 10, false) } else { vec![] };
            json!({"name": s("name", 80), "type": s("type", 80), "value": s("value", 160), "children": children})
        };
        shown.take(max).map(one).collect()
    }

    /// A snapshot of the stop as the verb's answer, frame `frame` (from 0) shown and named past F1;
    /// kept as `debug/<n>/s<k>.json` and as the session's last result, which `itse show dbg:N`
    /// shows. Its cost is the time since the call began, which `at` holds. An adapter that ended
    /// meanwhile leaves the session gone, not a snapshot that looks whole.
    fn answer(&self, caller: &Caller, scope: &str, stop: &Value, rule: Option<&str>, frame: usize, cancel: &Cancel) -> Result<Outcome> {
        let thread = stop["threadId"].as_i64().unwrap_or(lock(&self.at).0);
        let snap = self.snapshot(thread, stop["reason"].as_str().unwrap_or("pause"), cancel)?;
        self.alive(cancel)?;
        let shown = frame_at(&snap, frame)?["name"].as_str().map(|n| if frame > 0 { format!("F{} {n}", frame + 1) } else { n.into() });
        let mut at = lock(&self.at);
        let (k, ms) = (at.2 + 1, at.3.elapsed().as_millis() as u64);
        *at = (thread, snap.clone(), k, Instant::now());
        drop(at);
        let file = self.state.join("debug").join(&self.handle[4..]).join(format!("s{k}.json"));
        sys::write_atomic(&file, snap.to_string().as_bytes())?;
        let (body, cut) = text(&snap, frame);
        let reason = match snap["reason"].as_str().unwrap_or("") {
            "step" => "after a step",
            r => r,
        };
        let rule = rule.map_or(String::new(), |r| format!("; stop on: {r}"));
        let counts = format!("{reason} in {}, thread {}{rule}", shown.as_deref().unwrap_or("a frame outside the project"), snap["thread"]["name"]);
        // One note, so the page cap never splits it: the text is capped already.
        let cut = if cut { format!("\ntruncated; the whole snapshot: {}", file.display()) } else { String::new() };
        let note = format!("{body}{cut}\nmore: debug step|continue|eval|snapshot|stop {}", self.handle);
        let o = Outcome { id: Some(self.handle.clone()), cost: Cost { ms, warm: None }, notes: vec![note], ..Outcome::new("debug", Verdict::Stopped, scope, counts) };
        Store(self.state.clone()).save(&o, &caller.id.session)?;
        Ok(o)
    }

    /// Keep `why` (`is stopped`, `is gone: …`) for later calls, disconnect (ending a launched
    /// debuggee), and stop the adapter and what it started. Once: a reap and a stop may race.
    pub fn end(&self, host: &Host, why: &str) {
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        mark(&self.state, &self.handle, why);
        if self.peer.closed().is_none() {
            let _ = self.peer.request("disconnect", json!({"terminateDebuggee": self.launch}), Duration::from_secs(3), &Cancel::default());
        }
        if let Some(child) = &self.ends.child {
            let mut child = lock(child);
            let pid = child.id() as i32;
            host::kill_tree(pid, &marker(&self.handle), || drop(child.try_wait()));
            let _ = child.wait();
            host.untrack(&self.handle, pid);
        }
        let _ = self.ends.tcp.as_ref().map(|t| t.shutdown(Shutdown::Both));
        lock(&host.debug).remove(&self.handle);
    }
}

/// Frame `f` (from 0) of a snapshot; F1 always, as a stop may be outside the project.
fn frame_at(snap: &Value, f: usize) -> Result<&Value> {
    let n = snap["frames"].as_array().map_or(0, Vec::len);
    ensure!(f == 0 || f < n, "the stop has {n} project frames, so no F{}", f + 1);
    Ok(&snap["frames"][f])
}

/// A variable's line: name, type and value in columns.
fn var_line(v: &Value, indent: usize, name: usize, ty: usize) -> String {
    let s = |k: &str| v[k].as_str().unwrap_or("");
    format!("{:indent$}{:<name$}  {:<ty$}  {}", "", s("name"), s("type"), s("value")).trim_end().to_string()
}

/// A snapshot as text, frame `f` (from 0) shown with its source and variables: whole while it fits
/// in 4,000 chars, else without children, then without the later frames, then with fewer locals
/// (a `… N more locals` row), the location, the exception, the frames and the output always kept;
/// true if cut, or if the snapshot itself was.
fn text(snap: &Value, f: usize) -> (String, bool) {
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let (fr, frames) = (&snap["frames"][f], list(&snap["frames"]));
    let mut head = Vec::new();
    if let Some(path) = fr["path"].as_str() {
        let line = fr["line"].as_u64().unwrap_or(0);
        head.push(format!("{path}:{line}:{}", fr["col"].as_u64().unwrap_or(1)));
        let src = list(&fr["src"]).iter().map(|s| (s[0].as_u64().unwrap_or(0), s[1].as_str().unwrap_or("").to_string())).collect::<Vec<_>>();
        head.extend(src.iter().map(|(n, t)| format!("{}{n:>4} | {t}", if *n == line { "  >" } else { "   " })));
    }
    let locals = |children: bool| {
        let mut out = Vec::new();
        for scope in list(&fr["scopes"]) {
            let vars = list(&scope["vars"]);
            let width = |k: &str| vars.iter().map(|v| v[k].as_str().map_or(0, |s| s.chars().count())).max().unwrap_or(0).min(40);
            out.push(scope["name"].as_str().unwrap_or("").to_lowercase());
            for v in &vars {
                out.push(var_line(v, 3, width("name"), width("type")));
                out.extend(list(&v["children"]).iter().filter(|_| children).map(|c| var_line(c, 6, 0, 0)));
            }
        }
        out
    };
    let tail = |shown: usize| {
        let (e, mut out) = (&snap["exception"], Vec::new());
        out.extend(e.is_object().then(|| format!("exception {}: {}", e["exceptionId"].as_str().unwrap_or("?"), e["description"].as_str().unwrap_or(""))));
        out.push("frames (project)".into());
        out.extend(frames.iter().take(shown).zip(1..).map(|(x, i)| format!("   F{i} {}  {}:{}", x["name"].as_str().unwrap_or("?"), x["path"].as_str().unwrap_or(""), x["line"])));
        let (more, framework) = (frames.len().saturating_sub(shown), snap["framework"].as_u64().unwrap_or(0));
        out.extend((more > 0).then(|| format!("   … {more} more project frames")).into_iter().chain((framework > 0).then(|| format!("   … {framework} framework frames"))));
        let output = list(&snap["output"]);
        out.extend(output[output.len().saturating_sub(5)..].iter().map(|l| format!("output: {}", l.as_str().unwrap_or(""))));
        out
    };
    let size = |ls: &[String]| ls.iter().map(|l| l.chars().count() + 1).sum::<usize>();
    let tries = [(locals(true), tail(usize::MAX)), (locals(false), tail(usize::MAX)), (locals(false), tail(f + 1))];
    let n = tries.iter().position(|(l, t)| size(&head) + size(l) + size(t) <= CAP + 1).unwrap_or(2);
    let (mut vars, tail) = tries[n].clone();
    if n == 2 && size(&head) + size(&vars) + size(&tail) > CAP + 1 {
        // Room for the `… N more locals` row too; the locals' first lines are kept while they fit.
        let room = (CAP + 1).saturating_sub(size(&head) + size(&tail) + 24);
        let keep = vars.iter().scan(0, |n, l| Some(*n + l.chars().count() + 1).inspect(|end| *n = *end)).take_while(|end| *end <= room).count();
        let dropped = vars.drain(keep..).filter(|l| l.starts_with("   ")).count();
        vars.push(format!("   … {dropped} more locals"));
    }
    let mut t = [head, vars, tail].concat().join("\n");
    let end = t.char_indices().nth(CAP).map(|(i, _)| t[..i].rfind('\n').unwrap_or(i));
    t.truncate(end.unwrap_or(t.len()));
    (t, n > 0 || snap["cut"] == true)
}

/// What is left of a session: `<handle> <why>`, and where its last snapshot is, when it took one.
fn gone(state: &Path, handle: &str, why: &str) -> String {
    let last = Store(state.into()).load(handle).is_ok();
    format!("{handle} {why}{}", if last { format!("; its last snapshot: itse show {handle}") } else { String::new() })
}

/// Keep what became of a session in its folder, `debug/<n>/ended`, for later calls and the next
/// daemon; the first word kept stays. Its last snapshot, as `itse show` shows it, says so too, and
/// offers no more steps.
fn mark(state: &Path, handle: &str, why: &str) {
    let file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(state.join("debug").join(&handle[4..]).join("ended"));
    let (store, kept) = (Store(state.into()), file.and_then(|mut f| f.write_all(why.as_bytes())).is_ok());
    if let Some((mut o, session)) = store.load(handle).ok().filter(|_| kept) {
        (o.verdict, o.counts) = (Verdict::Exited, format!("{handle} {why}; its last stop: {}", o.counts));
        o.notes.iter_mut().for_each(|n| n.truncate(n.rfind("\nmore: debug ").unwrap_or(n.len())));
        let _ = store.save(&o, &session);
    }
}

/// A live session. One whose adapter ended is ended here; else what its folder kept of its end, or
/// that the daemon that held it is gone, is an error.
fn session(host: &Host, handle: &str) -> Result<Arc<Session>> {
    let (live, state) = (lock(&host.debug).get(handle).cloned(), host.ws.state_dir(&host.dirs));
    let dir = handle.strip_prefix("dbg:").filter(|n| n.parse::<u64>().is_ok()).map(|n| state.join("debug").join(n)).filter(|d| d.is_dir());
    if let Some(s) = live.as_ref().filter(|s| s.lost().is_none()) {
        return Ok(s.clone());
    }
    let why = match (&live, dir) {
        (Some(s), _) => s.lost().unwrap_or_default(),
        (None, Some(dir)) => fs::read_to_string(dir.join("ended")).unwrap_or_else(|_| "is gone: daemon restarted".into()),
        (None, None) => "is not running".into(),
    };
    live.inspect(|s| s.end(host, &why));
    bail!("{}", gone(&state, handle, &why))
}

/// `stop dbg:N` and `debug stop dbg:N`: `disconnect`, then the adapter is stopped. A session that
/// ended already is nothing to stop.
pub fn stop(host: &Host, verb: &str, handle: &str) -> Outcome {
    let known = handle.strip_prefix("dbg:").filter(|n| n.parse::<u64>().is_ok()).is_some_and(|n| host.ws.state_dir(&host.dirs).join("debug").join(n).is_dir());
    match session(host, handle) {
        Ok(s) => {
            s.end(host, "is stopped");
            Outcome::new(verb, Verdict::Stopped, handle, "stopped")
        }
        Err(e) if known => Outcome::new(verb, Verdict::Ok, handle, format!("nothing to stop: {e:#}")),
        Err(e) => Outcome::error(verb, "unknown_id", format!("{e:#}")),
    }
}

/// The daemon's tick: a session whose adapter ended, or unused for 10 minutes (`ITSE_DEBUG_IDLE_S`
/// in seconds, else), is ended on a thread of its own.
pub fn reap(host: &Arc<Host>) {
    let idle = std::env::var("ITSE_DEBUG_IDLE_S").ok().and_then(|s| s.parse().ok()).map_or(IDLE, Duration::from_secs);
    let mut all = lock(&host.debug);
    for s in all.values().filter(|s| fs::metadata(&s.log).is_ok_and(|m| m.len() > host::MAX_LOG)) {
        // Its stderr is opened to append, so the adapter writes on at the new end.
        let _ = fs::OpenOptions::new().write(true).open(&s.log).and_then(|f| f.set_len(0));
    }
    let done: Vec<String> = all.iter().filter(|(_, s)| s.peer.closed().is_some() || lock(&s.at).3.elapsed() >= idle).map(|(h, _)| h.clone()).collect();
    for s in done.iter().filter_map(|h| all.remove(h)) {
        let span = if idle.as_secs().is_multiple_of(60) { format!("{} minutes", idle.as_secs() / 60) } else { format!("{} s", idle.as_secs()) };
        let (host, why) = (host.clone(), s.lost().unwrap_or_else(|| format!("is gone: idle for {span}")));
        // Kept now, so a call before the end is done hears why.
        mark(&s.state, &s.handle, &why);
        thread::spawn(move || s.end(&host, &why));
    }
}

/// After a manifest verb: a workspace no longer trusted (revoked, or an extension changed what
/// runs) ends its sessions, which run project code.
pub fn untrusted(host: &Host, env: &BTreeMap<String, String>) {
    if !resolve::resolve(&host.ws, &host.dirs, env, SystemTime::now()).is_ok_and(|r| r.trusted) {
        let all: Vec<Arc<Session>> = lock(&host.debug).values().cloned().collect();
        all.iter().for_each(|s| s.end(host, "is stopped: the workspace is not trusted now"));
    }
}

/// Status rows: each declared adapter that is ready, and each live session, which holds an adapter.
pub fn rows(host: &Host, launches: &[Launch]) -> Vec<State> {
    let ready = launches.iter().filter(|l| l.protocol == Protocol::Dap).map(|l| (format!("dap:{}", l.id), "ready", "starts with debug test or attach".into(), None));
    let all: Vec<Arc<Session>> = lock(&host.debug).values().cloned().collect();
    let live = all.iter().map(|s| {
        let idle = lock(&s.at).3.elapsed().as_secs();
        let state = if lock(&s.pending).is_some() { "running" } else { "stopped" };
        (s.handle.clone(), state, format!("debug {}, idle {idle} s", s.scope), Some(vec!["stop".to_string(), s.handle.clone()]))
    });
    let row = |(item, state, reason, next): (String, &str, String, Option<Vec<String>>)| State {
        item,
        state: state.into(),
        reason: Some(reason),
        actor: next.as_ref().map(|_| "agent".into()),
        next,
        ..State::default()
    };
    ready.chain(live.collect::<Vec<_>>()).map(row).collect()
}

/// `debug test <target> [filter|run:N/T1] [--at L…] [--log L=text…]` and `debug attach
/// <host:port> [--at…]`: reach the adapter, shake hands, and run to the first stop the rule keeps.
/// A debug launch runs project code, and an attach sends the extension's `attach` arguments and
/// evaluates in the debuggee, so an untrusted workspace is UNTRUSTED before anything starts. A stop
/// not come by `until` leaves the session RUNNING.
#[allow(clippy::too_many_arguments)]
fn start(host: &Arc<Host>, caller: &Caller, verb: &str, args: &Args, scope: &str, started: Instant, until: Instant, cancel: &Cancel) -> Result<Outcome> {
    let r = resolve::resolve(&host.ws, &host.dirs, &caller.env, SystemTime::now())?;
    if !r.trusted {
        let states = r.states.into_iter().filter(|s| s.state == "untrusted").collect();
        let counts = format!("a debug {} runs project code", if verb == "test" { "launch" } else { "attach" });
        return Ok(Outcome { states, err: Some("untrusted"), ..Outcome::new("debug", Verdict::Untrusted, scope, counts) });
    }
    let debugs: Vec<_> = r.manifest.uses().iter().filter_map(|u| r.manifest.exts.get(&u.id)).filter_map(|e| Some((e.0.debug.as_ref()?, &e.0.servers))).collect();
    let (d, packages) = (debugs.iter().find(|d| !d.0.adapter.is_empty()), debugs.iter().flat_map(|d| d.0.project_packages.clone()).collect::<Vec<_>>());
    let l = d.and_then(|d| r.launches.iter().find(|l| l.protocol == Protocol::Dap && l.id == d.0.adapter));
    // The declared adapter's entry, ready or not: its `hide_vars` hold for an attach too.
    let server = d.and_then(|d| d.1.iter().find(|s| s.id == d.0.adapter));
    let (adapter, hide) = (server.and_then(|s| s.adapter_id.clone()), server.map_or(&[][..], |s| &s.hide_vars[..]));
    // `debug test [target] [filter] [run:N/Tk]`, the item in any place.
    let mut plain = args.rest.iter().filter(|a| named(a).is_none()).cloned();
    let (state, target, filter) = (host.ws.state_dir(&host.dirs), plain.next().unwrap_or_default(), plain.next().unwrap_or_default());
    ensure!(verb == "test" || !target.is_empty(), "missing argument <host:port>");
    let tests = format!("{} (", if target.is_empty() { host.ws.root.file_name().unwrap_or_default().to_string_lossy().into() } else { target.clone() });
    let rule = rule(&host.ws.root, &caller.cwd, &Store(state.clone()), args, (verb == "test").then_some(tests.as_str()), &packages)?;
    let env = env::workspace_env(&r, &host.dirs, caller.env.clone(), &store::Real(caller.env.clone()))?;
    let (d, picked) = match (d.map(|d| d.0), l) {
        (None, _) if verb == "test" => bail!("no extension in use declares how its tests run under a debugger (debug.adapter)"),
        (Some(d), None) if verb == "test" => bail!("the debug adapter {} is not ready; itse status shows why", d.adapter),
        (d, l) => (d, d.zip(l)),
    };
    let handle = Next::take(&state, "dbg")?;
    sys::private_dir(&state.join("debug").join(&handle[4..]))?;
    let log = state.join("debug").join(&handle[4..]).join("adapter.log");
    // An arguments object always, as DAP has it, though the extension leaves it out.
    let object = |v: Value| if v.is_null() { json!({}) } else { v };
    let inputs: BTreeMap<String, String> = [("target".to_string(), target.clone()), ("filter".into(), filter)].into();
    let mut fill = |k: &str| match k.split_once(':') {
        Some(("input", i)) => inputs.get(i).cloned(),
        _ => (k == "workspaceFolder").then(|| host.ws.root.display().to_string()),
    };
    let mut opened = || -> Result<(Stream, String, Value)> {
        if verb == "attach" {
            let (h, port) = address(&json!(target))?;
            return Ok((tcp(connect_loopback(&h, port, 50)?, Ends::default())?, "attach".into(), object(render_value(d.map_or(&Value::Null, |d| &d.attach), &mut fill)?)));
        }
        let (d, l) = picked.ok_or_else(|| anyhow!("no debug adapter"))?;
        let arguments = object(render_value(&d.arguments, &mut fill)?);
        Ok((open(host, &r, caller, l, &handle, &log, &env, cancel)?, d.request.clone().unwrap_or_else(|| "launch".into()), arguments))
    };
    let attached = opened().and_then(|((read, write, ends), request, arguments)| {
        let events = Arc::new(Events::default());
        let peer = Peer::start(read, write, Framing::Headers, Dialect::Dap, events.clone());
        let hide = hide.iter().filter_map(|h| Regex::new(h).ok()).collect();
        let (root, at, launch, caps, ended) = (host.ws.root.clone(), Mutex::new((1, Value::Null, 0, started)), request == "launch", OnceLock::new(), AtomicBool::new(false));
        let (pending, scope, log, state) = (Mutex::new(None), scope.to_string(), log.clone(), state.clone());
        let s = Arc::new(Session { handle: handle.clone(), peer, events, caps, ends, launch, root, state, hide, at, pending, ended, scope, log });
        let id = adapter.or(l.map(|l| l.id.clone())).unwrap_or_else(|| "itse".into());
        let h = s.handshake(&id, &request, arguments, &rule, d.and_then(|d| d.caught_condition.as_deref()), until, cancel);
        h.map(|h| (s.clone(), h)).inspect_err(|_| s.end(host, &s.lost().unwrap_or_else(|| FAILED.into())))
    });
    host.usage.record(&caller.env, &caller.id.sess, Event::Attach { outcome: if attached.is_ok() { "ok" } else { "fail" } });
    let (s, (plain, notes)) = attached.inspect_err(|_| mark(&state, &handle, FAILED))?;
    lock(&host.debug).insert(handle, s.clone());
    let ended = |_: &anyhow::Error| s.end(host, &s.lost().unwrap_or_else(|| "is stopped".into()));
    let (stop, skipped) = s.first_stop(plain, until, cancel).inspect_err(ended)?;
    let notes: Vec<String> = (skipped > 0).then(|| format!("{skipped} framework stops skipped")).into_iter().chain(notes).chain(s.ends.notes.clone()).collect();
    let text = if notes.is_empty() { rule.text } else { format!("{} ({})", rule.text, notes.join("; ")) };
    let text = if rule.logs.is_empty() { text } else { format!("{text}; logpoints at {}", rule.logs.join(", ")) };
    s.settle(host, caller, scope, stop, plain, Some(&text), 0, cancel).inspect_err(ended)
}

/// The `debug` verb: `test` and `attach` start a session; `step [over|in|out]`, `continue`, `eval
/// <expr> [--frame F]`, `snapshot [--frame F]` and `stop` act on one, `dbg:N`. A wait for a stop
/// ends after `wait`, RUNNING; `snapshot` waits for that stop then.
pub fn debug(host: &Arc<Host>, caller: &Caller, args: &Args, wait: Duration, cancel: &Cancel) -> Outcome {
    let (verb, started) = (args.target.as_deref().unwrap_or(""), Instant::now());
    let until = started + wait;
    let scope = [verb].into_iter().chain(args.rest.iter().map(String::as_str)).collect::<Vec<_>>().join(" ");
    let done = || -> Result<Outcome> {
        let frame = args.flags.get("frame").and_then(Value::as_str).map_or(Some(1), |f| f.trim_start_matches('F').parse().ok()).filter(|n: &usize| *n > 0);
        let frame = frame.ok_or_else(|| anyhow!("--frame takes F1, F2, …"))? - 1;
        match verb {
            "test" | "attach" => return start(host, caller, verb, args, &scope, started, until, cancel),
            "step" | "continue" | "eval" | "snapshot" | "stop" => {}
            _ => bail!("'debug' takes test, attach, step, continue, eval, snapshot or stop"),
        }
        let handle = args.rest.first().filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("missing argument <dbg>"))?;
        if verb == "stop" {
            return Ok(Outcome { scope: scope.clone(), ..stop(host, "debug", handle) });
        }
        let s = match session(host, handle) {
            Ok(s) => s,
            Err(e) => return Ok(Outcome { scope: scope.clone(), ..Outcome::error("debug", "unknown_id", format!("{e:#}")) }),
        };
        let mut at = lock(&s.at);
        let (thread, last) = (at.0, at.1.clone());
        at.3 = Instant::now();
        drop(at);
        let pending = *lock(&s.pending);
        if let Some(plain) = pending {
            ensure!(verb == "snapshot", "{handle} has not stopped yet; 'itse debug snapshot {handle}' waits for its stop");
            let stop = s.first_stop(plain, until, cancel)?.0;
            return s.settle(host, caller, &scope, stop, plain, None, frame, cancel);
        }
        let cmd = match (verb, args.rest.get(1).map(String::as_str)) {
            ("eval", _) => {
                let expr = args.rest[1..].join(" ");
                ensure!(!expr.is_empty(), "missing argument <expr>");
                let v = s.request("evaluate", json!({"expression": expr, "frameId": frame_at(&last, frame)?["id"], "context": "repl"}), ASK, cancel)?;
                let ty = v["type"].as_str().map(|t| format!(" ({t})")).unwrap_or_default();
                return Ok(Outcome::new("debug", Verdict::Ok, scope.clone(), clip(v["result"].as_str().unwrap_or(""), 1000) + &ty));
            }
            ("snapshot", _) => return s.answer(caller, &scope, &json!({"threadId": thread, "reason": last["reason"]}), None, frame, cancel),
            ("continue", _) => "continue",
            (_, None | Some("over")) => "next",
            (_, Some("in")) => "stepIn",
            (_, Some("out")) => "stepOut",
            (_, Some(other)) => bail!("'debug step' takes over, in or out, not '{other}'"),
        };
        s.resume(cmd, json!({"threadId": thread, "granularity": "line"}), cancel)?;
        let stop = s.wait_stop(until, (cmd != "continue").then_some(thread), cancel)?;
        s.settle(host, caller, &scope, stop, false, None, frame, cancel)
    };
    done().unwrap_or_else(|e| {
        let msg = format!("{e:#}");
        let err = CLASSES.iter().find(|(k, _)| msg.contains(k)).map_or("io", |c| c.1);
        Outcome { scope, ..Outcome::error("debug", err, msg) }
    })
}

#[cfg(test)]
mod tests;
