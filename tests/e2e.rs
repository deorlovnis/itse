//! End-to-end tests: the built `itse` binary, driven the way a shell or a harness drives it.
//!
//! Each test gets a private HOME and XDG dirs under /tmp/itse-e2e-<pid>-<n> (short, for sun_path),
//! with XDG_RUNTIME_DIR removed, so nothing touches the real ~/.config/itse or ~/.local/state/itse.
//! Daemons idle out after 2 s, and each test's `Env` stops every daemon it started when dropped.
//! `Mcp` drives `itse mcp` the way a harness does, over the relay's stdio.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{fs, process, thread};

use serde_json::{Value, json};

const ITSE: &str = env!("CARGO_BIN_EXE_itse");
const FIXTURE: &str = env!("CARGO_BIN_EXE_itse-fixture");

/// A private home for one test. Dropping it runs `itse daemon stop` for each daemon it started,
/// then kills the group of any that is left, and removes the folder.
struct Env {
    dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(format!("/tmp/itse-e2e-{}-{}", process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["home", "config", "data", "state", "cache", "work"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        Self { dir }
    }

    fn work(&self) -> PathBuf {
        fs::canonicalize(self.dir.join("work")).unwrap()
    }

    /// `itse ARGS` in `cwd`, with every path pointing into this test's folder.
    fn itse(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        self.command_of(ITSE, cwd, args)
    }

    fn command_of(&self, exe: &str, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .current_dir(cwd)
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("TMPDIR", &self.dir)
            .env("ITSE_DAEMON_IDLE_S", "2")
            .env("ITSE_SERVER_IDLE_S", "2")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("ITSE_WORKSPACE")
            .env_remove("ITSE_TEST_PROTO")
            .env_remove("ITSE_TEST_SLOW_MS")
            .env_remove("ITSE_TEST_STOP_MS")
            .stdin(Stdio::null());
        cmd
    }

    /// The state folder of each workspace that ever had a daemon: `(ws dir, daemon.json if present)`.
    fn daemons(&self) -> Vec<(PathBuf, Value)> {
        let Ok(dirs) = fs::read_dir(self.dir.join("state/itse/ws")) else { return vec![] };
        let read = |p: PathBuf| fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        dirs.flatten().map(|d| (d.path(), read(d.path().join("daemon.json")))).collect()
    }

    fn daemon_json(&self) -> Value {
        self.daemons().into_iter().map(|(_, info)| info).find(|info| !info.is_null()).unwrap_or(Value::Null)
    }

    fn socket(&self) -> PathBuf {
        self.daemons().first().expect("a daemon started").0.join("itse.sock")
    }

    /// `itse status` in the work folder, which must succeed; the daemon's pid.
    fn status_pid(&self) -> i32 {
        let out = self.itse(&self.work(), &["status"]);
        assert!(out.status.success(), "{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
        daemon_pid(&stdout(&out))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (dir, info) in self.daemons() {
            let root = fs::read_to_string(dir.join("root")).unwrap_or_default();
            if !root.is_empty() {
                let _ = self.command(&self.dir, &["--root", &root, "daemon", "stop"]).output();
            }
            if let Some(pid) = info["pid"].as_i64().filter(|&pid| alive(pid as i32)) {
                signal(-(pid as i32), "KILL");
            }
        }
        // The store is read-only; u+w comes back first so the folder can go.
        let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&self.dir).status();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The pid in `OK status <root> — daemon pid <pid>`.
fn daemon_pid(text: &str) -> i32 {
    let first = text.lines().next().unwrap_or_default();
    first.rsplit_once("daemon pid ").and_then(|(_, pid)| pid.parse().ok()).unwrap_or_else(|| panic!("no daemon pid in {text:?}"))
}

/// Field `n` (1-based, as in proc(5)) of /proc/<pid>/stat.
fn stat_field(pid: i32, n: usize) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    text[text.rfind(')')? + 2..].split(' ').nth(n - 3).map(str::to_string)
}

fn alive(pid: i32) -> bool {
    stat_field(pid, 3).is_some_and(|state| state != "Z" && state != "X")
}

fn signal(pid: i32, sig: &str) {
    let _ = Command::new("kill").args(["-s", sig, "--", &pid.to_string()]).stderr(Stdio::null()).status();
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while !done() {
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Processes running `itse --root <root> daemon`, from /proc.
fn daemon_processes(root: &Path) -> Vec<i32> {
    let want = format!("--root\0{}\0daemon\0", root.display());
    let pids = fs::read_dir("/proc").unwrap().flatten().filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok());
    pids.filter(|pid| fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&want)) && alive(*pid)).collect()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn version_prints_the_crate_version() {
    let env = Env::new();
    let out = env.itse(&env.dir, &["--version"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("itse {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_the_verbs() {
    let env = Env::new();
    for args in [&["help"][..], &["--help"], &[]] {
        let out = env.itse(&env.dir, args);
        assert!(out.status.success());
        assert!(stdout(&out).contains("  status      the workspace, its items and their states\n"), "{}", stdout(&out));
    }
}

#[test]
fn status_prints_the_workspace_and_repo_lines_from_the_daemon() {
    let env = Env::new();
    let root = env.work();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    let out = env.itse(&root.join("src/deep"), &["status"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with(&format!("OK status {} — daemon pid ", root.display())), "{text}");
    assert!(lines[1].starts_with(&format!("workspace: {} (ws ", root.display())), "{text}");
    assert!(lines[2].starts_with(&format!("repo: {} (rid ", root.display())) && lines[2].ends_with(&format!("git common dir {}/.git)", root.display())), "{text}");
    assert!(lines[3].starts_with(&format!("daemon: pid {}, started by cli ", daemon_pid(&text))), "{text}");
    let state = fs::canonicalize(env.dir.join("state")).unwrap();
    assert!(lines[4].starts_with(&format!("socket: {}/itse/ws/", state.display())), "{text}");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 2, "nothing was written into the workspace");
}

#[test]
fn status_without_an_absolute_home_is_exit_2_and_writes_nothing() {
    let env = Env::new();
    let work = env.dir.join("work");
    for home in [None, Some("relative")] {
        let mut cmd = env.command(&work, &["status"]);
        cmd.env_remove("XDG_STATE_HOME");
        match home {
            Some(home) => cmd.env("HOME", home),
            None => cmd.env_remove("HOME"),
        };
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "itse: HOME is not an absolute path and XDG_STATE_HOME is unset; itse keeps its state under them\n");
        assert_eq!(fs::read_dir(&work).unwrap().count(), 0, "no state lands in the workspace");
    }
}

#[test]
fn status_outside_git_names_the_folder_and_root_overrides_the_cwd() {
    let env = Env::new();
    let work = fs::canonicalize(env.dir.join("work")).unwrap();
    let out = env.itse(&env.dir, &["--root", work.to_str().unwrap(), "status"]);
    assert!(stdout(&out).lines().nth(1).unwrap().starts_with(&format!("workspace: {} (not a git work tree; ws ", work.display())), "{}", stdout(&out));
}

#[test]
fn an_unknown_verb_is_exit_2_with_one_lowercase_line() {
    let env = Env::new();
    let out = env.itse(&env.dir, &["tset"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "itse: unknown verb 'tset'; 'itse help' lists the verbs\n");
    let out = env.itse(&env.dir, &["--root", "/nonexistent-itse-root", "status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("itse: workspace /nonexistent-itse-root: "));
}

#[test]
fn the_fixture_binary_is_built_for_the_tests() {
    let out = Command::new(FIXTURE).arg("--version").output().unwrap();
    assert!(out.status.success());
    let out = Command::new(FIXTURE).arg("nonsense").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

// --- the daemon (design §3.3, §3.4) ---

#[test]
fn the_first_command_starts_the_daemon_and_the_second_reuses_it() {
    let env = Env::new();
    let first = env.status_pid();
    assert!(alive(first));
    assert_eq!(env.status_pid(), first);
    let info = env.daemon_json();
    assert_eq!((info["pid"].as_i64(), info["proto"].as_u64(), info["started_by"].as_str()), (Some(first.into()), Some(1), Some("cli")));
    assert_eq!(info["root"], json!(env.work()));
    let socket = env.socket();
    assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(socket.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn two_racing_commands_start_one_daemon() {
    let env = Env::new();
    let children: Vec<_> = (0..5).map(|_| env.command(&env.work(), &["status"]).stdout(Stdio::piped()).spawn().unwrap()).collect();
    let pids: Vec<i32> = children
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            daemon_pid(&stdout(&out))
        })
        .collect();
    assert!(pids.iter().all(|&p| p == pids[0]), "{pids:?}");
    assert!(wait_until(Duration::from_secs(3), || daemon_processes(&env.work()) == [pids[0]]), "{:?}", daemon_processes(&env.work()));
}

#[test]
fn a_command_during_shutdown_gets_a_new_daemon() {
    let env = Env::new();
    // The debug-only seam holds the old daemon in its stopping window, where servers stop from S4 on.
    let out = env.command(&env.work(), &["status"]).env("ITSE_TEST_STOP_MS", "1500").output().unwrap();
    let (old, socket) = (daemon_pid(&stdout(&out)), env.socket());
    signal(old, "TERM");
    // The stopping daemon unlinks its socket first and holds the lock until it exits.
    assert!(wait_until(Duration::from_secs(2), || !socket.exists()));
    assert!(alive(old), "the old daemon is still stopping");
    let started = Instant::now();
    let new = env.status_pid();
    assert_ne!(new, old);
    assert!(!alive(old), "the new daemon waited on the lock until the old one was gone");
    assert!(started.elapsed() >= Duration::from_millis(500), "{:?}", started.elapsed());
    assert_eq!(env.daemon_json()["pid"], new);
}

#[test]
fn the_daemon_exits_when_idle() {
    let env = Env::new();
    let pid = env.status_pid();
    let socket = env.socket();
    assert!(alive(pid));
    assert!(wait_until(Duration::from_secs(6), || !alive(pid)), "ITSE_DAEMON_IDLE_S=2");
    assert!(!socket.exists());
    assert!(env.daemon_json().is_null(), "daemon.json goes with the daemon");
}

#[test]
fn an_open_connection_keeps_the_daemon_alive() {
    let env = Env::new();
    let pid = env.status_pid();
    let conn = UnixStream::connect(env.socket()).unwrap();
    let mut writer = conn.try_clone().unwrap();
    writeln!(writer, "{}", json!({"jsonrpc": "2.0", "id": 0, "method": "hello", "params": {"proto": 1, "mode": "cli"}})).unwrap();
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).unwrap();
    thread::sleep(Duration::from_secs(4));
    assert!(alive(pid));
    drop(writer);
}

#[test]
fn sigterm_stops_an_idle_daemon_within_a_second() {
    let env = Env::new();
    let pid = env.status_pid();
    let socket = env.socket();
    signal(pid, "TERM");
    assert!(wait_until(Duration::from_secs(1), || !alive(pid)));
    assert!(!socket.exists());
}

#[test]
fn a_removed_socket_is_rebound() {
    let env = Env::new();
    let pid = env.status_pid();
    let socket = env.socket();
    fs::remove_file(&socket).unwrap();
    assert!(wait_until(Duration::from_secs(2), || socket.exists()));
    assert_eq!(env.status_pid(), pid);
}

#[test]
fn a_wedged_daemon_fails_hello_in_5_s() {
    let env = Env::new();
    let pid = env.status_pid();
    signal(pid, "STOP");
    let started = Instant::now();
    let out = env.itse(&env.work(), &["status"]);
    let took = started.elapsed();
    signal(pid, "CONT");
    assert_eq!(out.status.code(), Some(2));
    let expected = format!("itse: the daemon at {} did not answer hello in 5 s (pid {pid}); 'itse daemon stop' kills it\n", env.socket().display());
    assert_eq!(String::from_utf8_lossy(&out.stderr), expected);
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(8), "{took:?}");
}

#[test]
fn a_stale_socket_after_sigkill_is_replaced() {
    let env = Env::new();
    let old = env.status_pid();
    let socket = env.socket();
    signal(old, "KILL");
    assert!(wait_until(Duration::from_secs(2), || !alive(old)));
    assert!(socket.exists(), "SIGKILL leaves the socket file behind");
    let new = env.status_pid();
    assert_ne!(new, old);
}

#[test]
fn hello_is_required_first() {
    let env = Env::new();
    env.status_pid();
    let conn = UnixStream::connect(env.socket()).unwrap();
    writeln!(&conn, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "call", "params": {"verb": "status"}})).unwrap();
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32002, "message": "hello required"}}));
}

#[test]
fn a_proto_mismatch_restarts_the_daemon() {
    let env = Env::new();
    let old = env.status_pid();
    let out = env.command(&env.work(), &["status"]).env("ITSE_TEST_PROTO", "2").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let new = daemon_pid(&stdout(&out));
    assert_ne!(new, old);
    assert!(stdout(&out).contains("; proto 2; "), "{}", stdout(&out));
    assert!(wait_until(Duration::from_secs(2), || !alive(old)));
    assert_eq!(env.daemon_json()["proto"], 2);
}

#[test]
fn a_replaced_binary_restarts_an_idle_daemon() {
    let env = Env::new();
    let old = env.status_pid();
    let out = env.copy_status();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_ne!(daemon_pid(&stdout(&out)), old);
    assert_eq!(env.daemon_json()["exe_ino"].as_u64(), Some(fs::metadata(env.dir.join("itse-copy")).unwrap().ino()));
}

impl Env {
    /// `itse status` from a copy of the binary (another inode, as after a reinstall).
    fn copy_status(&self) -> Output {
        let copy = self.dir.join("itse-copy");
        fs::copy(ITSE, &copy).unwrap();
        // Another test's fork can hold the copy's write fd until its exec: ETXTBSY, briefly.
        let run = || self.command_of(copy.to_str().unwrap(), &self.work(), &["status"]).output();
        let mut out = run();
        for _ in 0..50 {
            if !matches!(&out, Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
            out = run();
        }
        out.unwrap()
    }
}

#[test]
fn the_daemon_is_not_a_child_of_its_starter() {
    let env = Env::new();
    let cli = env.command(&env.work(), &["status"]).stdout(Stdio::piped()).spawn().unwrap();
    let cli_pid = cli.id() as i32;
    let out = cli.wait_with_output().unwrap();
    let pid = daemon_pid(&stdout(&out));
    let ppid: i32 = stat_field(pid, 4).unwrap().parse().unwrap();
    assert!(ppid != cli_pid && ppid != process::id() as i32, "ppid {ppid}");
    assert_eq!(stat_field(pid, 6), Some(pid.to_string()), "the daemon leads its own session");
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), env.work());
}

#[test]
fn ctrl_c_in_the_cli_cancels_a_slow_query() {
    let env = Env::new();
    let pid = env.status_pid();
    let cli = env.command(&env.work(), &["status"]).env("ITSE_TEST_SLOW_MS", "30000").stdout(Stdio::piped()).spawn().unwrap();
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    signal(cli.id() as i32, "INT");
    let out = cli.wait_with_output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!((out.status.code(), stdout(&out)), (Some(130), "ERROR status — cancelled\n".to_string()));
    assert_eq!(env.status_pid(), pid, "the daemon goes on");
}

#[test]
fn daemon_status_shows_the_running_daemon_and_never_starts_one() {
    let env = Env::new();
    assert_eq!(stdout(&env.itse(&env.work(), &["daemon", "status"])), format!("no daemon runs for {}\n", env.work().display()));
    assert!(env.daemons().is_empty(), "no state was made");
    let pid = env.status_pid();
    let text = stdout(&env.itse(&env.work(), &["daemon", "status"]));
    assert!(text.starts_with(&format!("daemon pid {pid} {} started 20", env!("CARGO_PKG_VERSION"))) && text.contains(" by cli (env keys "), "{text}");
    let out = env.itse(&env.work(), &["daemon", "bogus"]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!((out.status.code(), err.as_str()), (Some(2), "itse: unexpected argument 'bogus'; 'daemon' takes stop, restart or status\n"));
}

#[test]
fn daemon_stop_stops_it_and_never_starts_one() {
    let env = Env::new();
    let out = env.itse(&env.work(), &["daemon", "stop"]);
    assert_eq!(stdout(&out), format!("no daemon runs for {}\n", env.work().display()));
    assert!(env.daemons().is_empty(), "no state was made");
    let pid = env.status_pid();
    let out = env.itse(&env.work(), &["daemon", "stop"]);
    assert_eq!(stdout(&out), format!("stopped daemon pid {pid}\n"));
    assert!(!alive(pid));
}

#[test]
fn the_env_guard_leaves_no_daemon() {
    let (pid, work) = {
        let env = Env::new();
        (env.status_pid(), env.work())
    };
    assert!(!alive(pid));
    assert!(daemon_processes(&work).is_empty());
}

// --- MCP through `itse mcp` (design §7.2–7.4) ---

/// An MCP client on a child's stdio, as a harness's would be (Halti's e2e `Mcp`).
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(cmd: &mut Command) -> Self {
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        Self { child, stdin: Some(stdin), stdout, next_id: 0 }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{message}").unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line:?}"))
    }

    /// A request and its result (or the error object).
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}));
        let reply = self.recv();
        assert_eq!(reply["id"], json!(self.next_id), "{reply}");
        reply.get("result").cloned().unwrap_or_else(|| reply["error"].clone())
    }

    fn handshake(&mut self, name: &str) -> Value {
        let init = self.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": name, "version": "0"}}));
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    fn tool_names(&mut self) -> Vec<String> {
        let list = self.rpc("tools/list", json!({}));
        list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    }

    /// Call a tool; returns (is_error, text).
    fn call(&mut self, name: &str, args: Value) -> (bool, String) {
        let result = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        (result["isError"].as_bool().unwrap(), result["content"][0]["text"].as_str().unwrap().to_string())
    }

    /// Close stdin and wait: the relay exits 0 when its client goes.
    fn close(mut self) -> (Option<i32>, String) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        let mut err = String::new();
        let _ = self.child.stderr.take().unwrap().read_to_string(&mut err);
        (status.code(), err)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Env {
    fn mcp(&self, extra: &[(&str, &str)]) -> Mcp {
        let mut cmd = self.command(&self.work(), &["mcp"]);
        cmd.envs(extra.iter().copied());
        Mcp::start(&mut cmd)
    }

    /// The usage log's records.
    fn usage(&self) -> Vec<Value> {
        let text = fs::read_to_string(self.dir.join("state/itse/usage.jsonl")).unwrap_or_default();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// Store a result of `items` test items as `q:<n>` for `session`, as the daemon stores one.
    fn plant(&self, n: u32, items: &[&str], session: &str) {
        let dir = self.daemons().first().expect("a daemon ran").0.join("results");
        fs::create_dir_all(&dir).unwrap();
        let items: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(i, id)| {
                json!({"id": id, "kind": "test", "loc": {"path": "src/lib.rs", "line": i + 1, "col": 5}, "src": [[i + 1, "    assert!(ok);"]],
                "msg": format!("tests::case_{i}"), "detail": ["assertion failed: ok"], "sources": []})
            })
            .collect();
        let count = format!("{} failed", items.len());
        let o = json!({"itse": 1, "session": session, "verb": "test", "verdict": "fail", "scope": "itse", "counts": count, "cost": {"ms": 1200, "warm": null},
            "id": format!("q:{n}"), "items": items, "states": [], "notes": [], "full": {"bytes": 0, "lines": 0}, "page": {"n": 1, "of": 1}});
        fs::write(dir.join(format!("{n}.json")), o.to_string()).unwrap();
    }
}

#[test]
fn the_fixture_serves_mcp_echo_and_slow_and_a_tool() {
    let env = Env::new();
    let mut mcp = Mcp::start(&mut env.command_of(FIXTURE, &env.dir, &["mcp"]));
    assert_eq!(mcp.handshake("e2e")["protocolVersion"], "2025-06-18");
    assert_eq!(mcp.tool_names(), ["echo", "slow"]);
    assert_eq!(mcp.call("echo", json!({"text": "hi"})), (false, "hi".to_string()));
    mcp.send(&json!({"jsonrpc": "2.0", "id": 50, "method": "tools/call", "params": {"name": "slow", "arguments": {"ms": 300}}}));
    assert_eq!(mcp.rpc("ping", json!({})), json!({}), "answered while slow runs");
    assert_eq!(mcp.recv()["result"]["content"][0]["text"], "slept 300 ms");
    let out = env.command_of(FIXTURE, &env.dir, &["tool", "tar", "-x", "-f", "a.tar"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((v["tool"].as_str(), v["argv"].clone()), (Some("tar"), json!(["-x", "-f", "a.tar"])));
    let w = "%{http_connect} %{http_code} %{url_effective}";
    let out = env.command_of(FIXTURE, &env.dir, &["tool", "curl", "--fail-with-body", "-sS", "-w", w, "-o", "x", "https://x.test/a"]).output().unwrap();
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(22), "200 404 https://x.test/a"), "an unserved URL is a 404");
    let out = env.command_of(FIXTURE, &env.dir, &["tool", "curl"]).env("ITSE_FIXTURE_MODE", "exit:56").output().unwrap();
    assert_eq!(out.status.code(), Some(56));
}

#[test]
fn itse_mcp_relays_the_three_tools_and_starts_the_daemon_as_mcp() {
    let env = Env::new();
    let mut mcp = env.mcp(&[]);
    let init = mcp.handshake("e2e");
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert!(init["instructions"].as_str().unwrap().starts_with("itse runs this workspace's declared tests"));
    assert_eq!(mcp.tool_names(), ["test_build_run", "problems_symbols", "show_result"]);
    let (is_error, text) = mcp.call("problems_symbols", json!({"verb": "status"}));
    assert!(!is_error && text.starts_with(&format!("OK status {} — daemon pid ", env.work().display())), "{text}");
    assert!(text.contains("\nmcp tools: 3 (~") && text.contains("debug tool off"), "{text}");
    assert_eq!(env.daemon_json()["started_by"], "mcp");
    assert_eq!(mcp.close(), (Some(0), String::new()));
}

#[test]
fn the_relay_answers_an_in_flight_call_and_replays_initialize_after_a_daemon_kill() {
    let env = Env::new();
    let mut mcp = env.mcp(&[("ITSE_TEST_SLOW_MS", "30000")]);
    mcp.handshake("e2e");
    let old = env.daemon_json()["pid"].as_i64().unwrap() as i32;
    mcp.send(&json!({"jsonrpc": "2.0", "id": "slow-1", "method": "tools/call", "params": {"name": "problems_symbols", "arguments": {"verb": "status"}}}));
    mcp.send(&json!({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"}));
    assert_eq!(mcp.recv()["id"], "ping-1", "the slow call is in flight");
    signal(old, "KILL");
    let reply = mcp.recv();
    assert_eq!(reply["id"], "slow-1");
    let text = "ERROR problems_symbols — itse daemon restarted; the call did not finish; call again";
    assert_eq!(reply["result"], json!({"content": [{"type": "text", "text": text}], "isError": true}), "a tool result, not an rpc error");
    assert_eq!(mcp.tool_names(), ["test_build_run", "problems_symbols", "show_result"], "the session goes on; the replay's reply was swallowed");
    let new = env.daemon_json()["pid"].as_i64().unwrap() as i32;
    assert!(new != old && alive(new));
    let inits: Vec<Value> = env.usage().into_iter().filter(|r| r["ev"] == "init").collect();
    assert_eq!(inits.len(), 2, "initialize was replayed to the new daemon");
    assert!(inits.iter().all(|r| r["client"] == "e2e" && r["proto"] == "2025-06-18"));
}

#[test]
fn a_relay_survives_a_cli_from_a_replaced_binary() {
    let env = Env::new();
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    let pid = env.daemon_json()["pid"].as_i64().unwrap() as i32;
    let out = env.copy_status();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(daemon_pid(&stdout(&out)), pid, "the session's daemon serves the newer cli");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(mcp.tool_names(), ["test_build_run", "problems_symbols", "show_result"]);
    assert_eq!(env.daemon_json()["pid"].as_i64(), Some(pid as i64));
    assert_eq!(mcp.close(), (Some(0), String::new()));
}

#[test]
fn the_relay_without_a_daemon_says_why_and_exits_1() {
    let env = Env::new();
    fs::create_dir_all(env.dir.join("state/itse")).unwrap();
    fs::write(env.dir.join("state/itse/ws"), "a file where the workspace dirs go").unwrap();
    let (code, err) = env.mcp(&[]).close();
    assert_eq!(code, Some(1));
    assert!(err.starts_with("itse mcp: ") && err.contains("/state/itse/ws") && err.lines().count() == 1, "{err}");
}

#[test]
fn cli_and_mcp_print_identical_text() {
    let env = Env::new();
    env.status_pid();
    let ids: Vec<String> = (1..=41).map(|i| format!("T{i}")).collect();
    env.plant(1, &ids.iter().map(String::as_str).collect::<Vec<_>>(), "cli");
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    for (cli, args) in
        [(vec!["show", "q:1"], json!({"id": "q:1"})), (vec!["show", "q:1", "--page", "2"], json!({"id": "q:1", "page": 2})), (vec!["show", "q:1/T7"], json!({"id": "q:1/T7"}))]
    {
        let out = env.itse(&env.work(), &cli);
        assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
        assert_eq!(mcp.call("show_result", args), (false, stdout(&out)), "{cli:?}");
    }
    assert!(stdout(&env.itse(&env.work(), &["show", "q:1"])).contains("; full: itse show q:1 --page 2\n"));
    let out = env.itse(&env.work(), &["show", "q:1", "--page", "99"]);
    assert_eq!((out.status.code(), mcp.call("show_result", json!({"id": "q:1", "page": 99})).0), (Some(2), true), "an error on both");
}

#[test]
fn a_bare_item_id_resolves_from_the_cli() {
    let env = Env::new();
    env.status_pid();
    env.plant(1, &["T1", "T2"], "cli");
    env.plant(2, &["T1"], "9f3a1c7e5d2b4410");
    let out = env.itse(&env.work(), &["show", "T1"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("FAIL test itse — 2 failed; 1.2 s; q:1\nT1 test src/lib.rs:1:5 tests::case_0\n"), "a plain shell's own result: {}", stdout(&out));
    assert!(stdout(&out).ends_with("T1 from q:1, the newest result holding it\n"));
    let out = env.command(&env.work(), &["show", "T1"]).env("ITSE_SESSION", "another").output().unwrap();
    assert!(stdout(&out).contains("T1 from q:2,"), "else the workspace's newest: {}", stdout(&out));
    let out = env.itse(&env.work(), &["show", "T9"]);
    assert_eq!((out.status.code(), stdout(&out)), (Some(2), "ERROR show T9 — no stored result holds T9\n".to_string()));
}

// --- the usage log (design §8) ---

/// Every key and string a record may hold (Halti audit.rs:396-408).
fn check_record(r: &Value) {
    let re = |p: &str, s: &str| regex_lite::Regex::new(p).unwrap().is_match(s);
    let keys = [
        "v",
        "ev",
        "ts",
        "ws",
        "sess",
        "via",
        "client",
        "client_v",
        "verb",
        "ext",
        "ms",
        "outcome",
        "err",
        "bytes",
        "full",
        "trunc",
        "warm",
        "settled",
        "proto",
        "tools",
        "list_bytes",
    ];
    let verbs = ["status", "show", "find", "test", "verbs", "unknown", "stats", "integrate", "ext", "trust", "mcp", "daemon"];
    let errs = [
        "bad_args",
        "unknown_verb",
        "unknown_id",
        "untrusted",
        "needs_human",
        "needs_consent",
        "not_installed",
        "server_start",
        "server_crash",
        "blocked_by_door",
        "timeout",
        "io",
        "internal",
    ];
    for (k, v) in r.as_object().unwrap() {
        assert!(keys.contains(&k.as_str()), "key {k} in {r}");
        let s = v.as_str().unwrap_or("");
        let ok = match k.as_str() {
            "ev" => ["init", "call"].contains(&s),
            "ts" => re(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$", s),
            "ws" => re("^[0-9a-f]{12}$", s),
            "sess" => re("^[0-9a-f]{16}$", s),
            "via" => ["cli", "mcp", "hook"].contains(&s),
            "client" | "client_v" => v.is_null() || (s.chars().count() <= 64 && !s.chars().any(char::is_control)),
            "verb" => verbs.contains(&s),
            "outcome" => ["ok", "fail", "error", "cancel", "timeout"].contains(&s),
            "err" => v.is_null() || errs.contains(&s),
            "proto" => ["2025-11-25", "2025-06-18"].contains(&s),
            "ext" => v.is_null(),
            _ => !v.is_string(),
        };
        assert!(ok, "{k} = {v} in {r}");
    }
}

#[test]
fn usage_lines_hold_no_values() {
    let env = Env::new();
    let secrets = [("ITSE_CLIENT", "evil\x1b[31mtool/9\x07"), ("CLAUDE_CODE_SESSION_ID", "zqx-claude-session-0123"), ("ITSE_SESSION", "zqx-session")];
    let cli = |args: &[&str]| env.command(&env.work(), args).envs(secrets).output().unwrap();
    let body = "fn main() { let zqx_body = \"sk-ant-zqx-0123\"; }";
    for args in [
        &["status"][..],
        &["status", "sk-ant-zqx-0123"],
        &["show", "/work/secret/Thing.kt"],
        &["find", "OrderRepository"],
        &["test", "saves_order"],
        &["show", body],
        &["show", "q:1", "--grep", "zqx"],
    ] {
        cli(args);
    }
    let mut mcp = env.mcp(&secrets);
    mcp.rpc("initialize", json!({"protocolVersion": "2025-11-25", "clientInfo": {"name": "agent\x1b]0;zqx\x07", "version": "zqx-1"}}));
    mcp.call("problems_symbols", json!({"verb": "find", "target": "OrderRepository", "args": ["--kind", "zqx"]}));
    mcp.call("test_build_run", json!({"verb": "tset-zqx", "args": [body]}));
    mcp.call("show_result", json!({"id": "/work/secret/Thing.kt", "grep": "zqx"}));
    mcp.call("problems_symbols", json!({"verb": "ext", "args": ["add", "/work/secret/zqx.json"]}));
    drop(mcp);
    // Without ITSE_CLIENT the client's own name is logged, clipped.
    let mut bare = env.mcp(&secrets[1..]);
    bare.rpc("initialize", json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "agent\x1b]0;evil\x07", "version": "1"}}));
    bare.call("problems_symbols", json!({"verb": "status"}));
    drop(bare);
    let conn = UnixStream::connect(env.socket()).unwrap();
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let hook_env: serde_json::Map<String, Value> = secrets.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    for (id, msg) in [
        (0, json!({"method": "hello", "params": {"proto": 1, "mode": "hook", "env": hook_env, "cwd": "/work/secret"}})),
        (1, json!({"method": "call", "params": {"verb": "tset-zqx", "args": [body]}})),
    ] {
        let mut msg = msg;
        (msg["jsonrpc"], msg["id"]) = (json!("2.0"), json!(id));
        writeln!(&conn, "{msg}").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
    }
    let log = fs::read_to_string(env.dir.join("state/itse/usage.jsonl")).unwrap();
    for needle in ["zqx", "sk-ant", "/work/secret", "Thing.kt", "OrderRepository", "saves_order", "fn main", "\\u001b", "\u{1b}", "\\u0007", "invalid option", "not built"] {
        assert!(!log.contains(needle), "{needle:?} in the usage log:\n{log}");
    }
    let records = env.usage();
    // The CLI refuses `status sk-ant-…` itself; the other 6 CLI calls, 5 + 2 MCP records and the hook's call reach the log.
    assert_eq!(records.len(), 14, "{records:?}");
    records.iter().for_each(check_record);
    let vias: std::collections::BTreeSet<&str> = records.iter().filter_map(|r| r["via"].as_str()).collect();
    assert_eq!(vias.into_iter().collect::<Vec<_>>(), ["cli", "hook", "mcp"]);
    assert!(records.iter().any(|r| r["verb"] == "unknown" && r["err"] == "unknown_verb"));
    assert!(records.iter().any(|r| r["client"] == "evil?[31mtool" && r["client_v"] == "9?"), "ITSE_CLIENT, clipped");
    assert_eq!(records.iter().filter(|r| r["client"] == "agent?]0;evil?").count(), 2, "clientInfo, clipped, on the init and the call");
}

#[test]
fn itse_usage_off_in_the_relay_env_writes_nothing() {
    let env = Env::new();
    env.status_pid();
    let before = env.usage().len();
    assert_eq!(before, 1);
    let mut mcp = env.mcp(&[("ITSE_USAGE", "off")]);
    mcp.handshake("e2e");
    mcp.call("problems_symbols", json!({"verb": "status"}));
    mcp.close();
    assert_eq!(env.usage().len(), before, "not even the init record");
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    mcp.close();
    assert_eq!(env.usage().len(), before + 1, "on again for the next session");
}

#[test]
fn config_off_writes_nothing() {
    let env = Env::new();
    fs::create_dir_all(env.dir.join("config/itse")).unwrap();
    fs::write(env.dir.join("config/itse/config.json"), r#"{"usage": false}"#).unwrap();
    env.status_pid();
    let mut mcp = env.mcp(&[]);
    mcp.handshake("e2e");
    mcp.call("problems_symbols", json!({"verb": "status"}));
    mcp.close();
    env.itse(&env.work(), &["verbs"]);
    assert!(!env.dir.join("state/itse/usage.jsonl").exists());
    let status = stdout(&env.itse(&env.work(), &["status"]));
    assert!(status.contains("\nusage log: off; ITSE_USAGE=off or \"usage\": false in "), "{status}");
}

#[test]
fn an_unwritable_state_dir_still_answers() {
    let env = Env::new();
    fs::create_dir_all(env.dir.join("state/itse/usage.jsonl")).unwrap();
    env.status_pid();
    env.status_pid();
    let log = fs::read_to_string(env.daemons()[0].0.join("daemon.log")).unwrap();
    assert_eq!(log.matches("itse daemon: usage log: ").count(), 1, "one line per lifetime: {log}");
    assert!(env.itse(&env.work(), &["verbs"]).status.success());
}

#[test]
fn stats_calls_are_not_counted() {
    let env = Env::new();
    env.status_pid();
    env.itse(&env.work(), &["verbs"]);
    let records = env.usage();
    assert_eq!(records.iter().map(|r| r["verb"].as_str().unwrap()).collect::<Vec<_>>(), ["status", "verbs"], "a local verb is recorded too");
    let out = env.itse(&env.work(), &["stats"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.starts_with("usage ") && text.contains(" (7d): 2 calls, 2 sessions, 1 workspaces; log "), "{text}");
    assert!(text.contains("\nstatus ") && text.contains("\nverbs "), "{text}");
    env.itse(&env.work(), &["stats", "--json"]);
    assert_eq!(env.usage().len(), 2, "stats reads and is not logged");
    let out = env.itse(&env.work(), &["stats", "--by", "colour"]);
    assert_eq!((out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned()), (Some(2), "itse: --by takes verb, ext, client, ws or day, not 'colour'\n".into()));
}

// --- the verb index (design §7.5) ---

#[test]
fn docs_verbs_md_equals_itse_verbs_md() {
    let env = Env::new();
    let out = env.itse(&env.work(), &["verbs", "--md"]);
    assert!(out.status.success());
    let golden = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/verbs.md")).unwrap();
    assert_eq!(stdout(&out), golden, "regenerate with: itse verbs --md > docs/verbs.md");
    let index = stdout(&env.itse(&env.work(), &["verbs"]));
    assert!(index.contains("itse test [target] [filter] [--action V]\n") && !index.contains("integrate") && !index.contains("trust"), "{index}");
    let v: Value = serde_json::from_slice(&env.itse(&env.work(), &["verbs", "--json"]).stdout).unwrap();
    assert!(v["verbs"].as_array().unwrap().iter().any(|v| v["name"] == "show" && v["tool"] == "show_result"));
    assert!(env.daemons().is_empty(), "the index never starts a daemon");
}

// --- opt-in: Claude Code (design §12.3) ---

#[test]
#[ignore = "needs Claude Code on PATH, zero spend: ITSE_E2E_CLAUDE=1"]
fn claude_code_lists_itse_over_mcp() {
    if std::env::var("ITSE_E2E_CLAUDE").as_deref() != Ok("1") {
        return eprintln!("skipped: ITSE_E2E_CLAUDE=1 runs it");
    }
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    // A throwaway Claude Code config; auth stays the box's (decision 15), nothing is copied.
    let claude = |args: &[&str]| env.command_of("claude", &env.work(), args).env("CLAUDE_CONFIG_DIR", env.dir.join("claude")).output().unwrap();
    let server = json!({"type": "stdio", "command": ITSE, "args": ["mcp"]}).to_string();
    let added = claude(&["mcp", "add-json", "itse", &server, "--scope", "local"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let list = claude(&["mcp", "list"]);
    let text = stdout(&list);
    eprintln!("claude mcp list:\n{text}");
    assert!(text.lines().any(|l| l.starts_with("itse:") && l.contains("Connected")), "{text}");
    let init = env.usage().into_iter().find(|r| r["ev"] == "init").expect("an init record from Claude Code's session");
    eprintln!("negotiated protocol {} with {} {}", init["proto"], init["client"], init["client_v"]);
    assert!(["2025-11-25", "2025-06-18"].contains(&init["proto"].as_str().unwrap()));
    // list_changed: `claude mcp list` connects once and lists once; whether a session re-lists on
    // notifications/tools/list_changed needs a model turn (ITSE_E2E_CLAUDE_PROBE), so it is recorded only.
    eprintln!("list_changed: not observable without a model turn; tools/list calls seen: {}", env.usage().iter().filter(|r| r["ev"] == "init").count());
}

// --- the manifest, layers, resolver and human-only verbs (S2, design §4, §10) ---

#[path = "e2e/resolver.rs"]
mod resolver;
#[path = "e2e/store.rs"]
mod store;

// --- actions, runs and the session host (S3, design §5.2, §5.3, §5.6) ---

#[path = "e2e/actions.rs"]
mod actions;
#[path = "e2e/cargo.rs"]
mod cargo;
