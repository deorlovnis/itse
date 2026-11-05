//! The workspace daemon: one per workspace, started on demand by the CLI, and
//! the keeper of warm servers and results. It listens on a 0600 Unix socket and speaks one JSON
//! value per line: `hello` first, then `call`, `ping` or `shutdown`.
//!
//! One instance per workspace is an flock on `daemon.lock`. The main thread ticks: it stops the
//! daemon on a signal, when idle or when the root is gone, and re-binds a socket someone removed.
//! glibc's `signal` sets SA_RESTART, so a blocked `accept` never sees a signal; a stop wakes it by
//! connecting to the socket itself.

pub mod client;

use std::fs::{self, OpenOptions, Permissions, TryLockError};
use std::io::{BufReader, ErrorKind};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use std::{env, mem, process, thread};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::host::Host;
use crate::mcp;
use crate::rpc::{self, Cancel, Framing};
use crate::sys::{self, lock};
use crate::verbs::{self, Caller};
use crate::workspace::{Dirs, Workspace};

/// ITSE-RPC's version. `hello` and `shutdown` never change across versions.
pub const PROTO: u64 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// The protocol this binary speaks; debug builds take `ITSE_TEST_PROTO`, to test a mismatch.
pub fn proto() -> u64 {
    env::var("ITSE_TEST_PROTO").ok().filter(|_| cfg!(debug_assertions)).and_then(|p| p.parse().ok()).unwrap_or(PROTO)
}

/// `(dev, inode)` of the running binary: a rebuilt or reinstalled one has another inode.
pub fn exe_id() -> (u64, u64) {
    fs::metadata("/proc/self/exe").map_or((0, 0), |m| (m.dev(), m.ino()))
}

/// `itse daemon`: serve the workspace until a signal, a `shutdown`, idleness or a removed root.
pub fn run(ws: Workspace, dirs: Dirs) -> Result<()> {
    sys::limit_arenas();
    sys::catch_signals();
    let (state, socket) = (ws.state_dir(&dirs), ws.socket(&dirs, Path::new("/tmp"))?);
    sys::private_dir(&state)?;
    // A daemon that is shutting down holds the lock for up to 3 s; a live one answers on the socket.
    let lock_file = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(state.join("daemon.lock"))?;
    let end = Instant::now() + Duration::from_secs(10);
    while let Err(e) = lock_file.try_lock() {
        match e {
            TryLockError::WouldBlock if UnixStream::connect(&socket).is_ok() => return Ok(()),
            TryLockError::WouldBlock if Instant::now() < end => thread::sleep(Duration::from_millis(100)),
            _ => bail!("{}/daemon.lock is held and {} doesn't answer", state.display(), socket.display()),
        }
    }
    crate::store::clean(&dirs);
    let host = Arc::new(Host::new(ws, dirs, socket, exe_id()));
    host.restart();
    bind(&host)?;
    let (pid, root) = (process::id(), host.ws.root.display());
    let info = json!({"pid": pid, "start_time": sys::proc_stat(pid as i32).map(|s| s.start_time), "proto": proto(), "version": VERSION, "exe_dev": host.exe.0,
        "exe_ino": host.exe.1, "started_at": sys::utc(host.started), "started_by": host.started_by, "env_keys_sha": host.env_keys_sha(), "root": host.ws.root});
    sys::write_atomic(&state.join("daemon.json"), info.to_string().as_bytes())?;
    sys::write_atomic(&state.join("root"), host.ws.root.as_os_str().as_encoded_bytes())?;
    eprintln!("{} itse daemon {VERSION} pid {pid} serves {root}", sys::utc(host.started));
    while !host.stopping.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(250));
        if sys::signalled() || host.idle() || !host.ws.root.is_dir() {
            stop(&host);
        } else if fs::symlink_metadata(&host.socket).ok().map(|m| m.ino()) != Some(*lock(&host.bound)) {
            bind(&host).unwrap_or_else(|e| eprintln!("itse daemon: re-binding the socket: {e:#}"));
        }
    }
    // The listener is closed and the socket gone. Hosted servers will stop here, for up to 3 s, which a
    // debug-only seam stands in for now; runs are detached and go on, re-adopted by the next daemon
    // from procs.json. Exiting drops the lock.
    thread::sleep(Duration::from_millis(env::var("ITSE_TEST_STOP_MS").ok().filter(|_| cfg!(debug_assertions)).and_then(|ms| ms.parse().ok()).unwrap_or(0)));
    let _ = fs::remove_file(state.join("daemon.json"));
    eprintln!("{} itse daemon pid {pid} stopped", sys::utc(SystemTime::now()));
    Ok(())
}

/// Bind the socket (again) and accept on it, one thread per connection. After a re-bind the old
/// accept thread stays blocked on a listener nobody can reach.
fn bind(host: &Arc<Host>) -> Result<()> {
    let mut bound = lock(&host.bound);
    if host.stopping.load(Ordering::SeqCst) {
        return Ok(());
    }
    let _ = fs::remove_file(&host.socket);
    let listener = UnixListener::bind(&host.socket).with_context(|| format!("binding {}", host.socket.display()))?;
    fs::set_permissions(&host.socket, Permissions::from_mode(0o600))?;
    *bound = fs::symlink_metadata(&host.socket).map_or(0, |m| m.ino());
    let host = host.clone();
    thread::spawn(move || {
        // A stop wakes this loop, which drops the listener: connections still queued see EOF.
        for (conn, host) in listener.incoming().flatten().take_while(|_| !host.stopping.load(Ordering::SeqCst)).map(|c| (c, host.clone())) {
            thread::spawn(move || serve_conn(conn, &host));
        }
    });
    Ok(())
}

/// Stop taking connections: wake the accept loop so it drops the listener, and unlink the socket,
/// so the next CLI starts a new daemon, which waits on the lock until this one has gone.
pub fn stop(host: &Host) {
    let bound = lock(&host.bound);
    if !host.stopping.swap(true, Ordering::SeqCst) {
        let _ = UnixStream::connect(&host.socket);
        if fs::symlink_metadata(&host.socket).is_ok_and(|m| m.ino() == *bound) {
            let _ = fs::remove_file(&host.socket);
        }
    }
}

/// One connection: `hello` within 5 s of the accept, however much else it sends, then requests
/// until EOF. The `call` runs on a scoped thread while this one reads on, so a `$/cancelRequest` or
/// the client's EOF cancels it.
pub fn serve_conn(stream: UnixStream, host: &Arc<Host>) {
    let _active = host.enter();
    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else { return };
    let (writer, cancel, mut caller, hello_by) = (Mutex::new(stream), Cancel::default(), None::<Caller>, Instant::now() + HELLO_WAIT);
    thread::scope(|scope| {
        let mut called = false;
        loop {
            // A zero timeout is an error: the hello wait is over.
            if caller.is_none() && lock(&writer).set_read_timeout(Some(hello_by.saturating_duration_since(Instant::now()))).is_err() {
                break;
            }
            let mut msg = match rpc::read(&mut reader, Framing::Lines, rpc::MAX_FRAME) {
                Ok(Some(msg)) => msg,
                Err(e) if e.kind() == ErrorKind::InvalidData => json!({"bad": e.to_string()}),
                _ => break,
            };
            let (id, params, method) = (msg["id"].take(), msg["params"].take(), msg["method"].as_str().unwrap_or("").to_string());
            let reply = match method.as_str() {
                // JSON that is no object is an invalid request; a frame that isn't JSON, a parse error.
                _ if !msg["bad"].is_null() => {
                    Err(json!({"code": if msg["bad"].as_str().is_some_and(|e| e.starts_with("not JSON")) { -32700 } else { -32600 }, "message": msg["bad"]}))
                }
                "hello" => hello(host, params, &writer).inspect(|(c, _)| caller = Some(c.clone())).map(|(_, reply)| reply),
                // Stopped (and unlinked) before the reply, so a CLI that starts a new daemon next doesn't find this one.
                "shutdown" => Ok(json!({"stopping": true})).inspect(|_| stop(host)),
                _ if caller.is_none() => Err(json!({"code": -32002, "message": "hello required"})),
                "ping" => Ok(json!({})),
                "$/cancelRequest" => Ok(Value::Null).inspect(|_| cancel.1.store(true, Ordering::SeqCst)).inspect(|_| cancel.cancel()),
                // One call per connection: the first sets `called`.
                "call" if !mem::replace(&mut called, true) => {
                    let (c, cancel, writer) = (caller.clone().unwrap_or_default(), &cancel, &writer);
                    scope.spawn(move || {
                        let out = panic::catch_unwind(AssertUnwindSafe(|| verbs::call(host, &c, &params, cancel)));
                        respond(writer, id, Ok(out.unwrap_or_else(|_| json!({"exit": 2, "text": "ERROR call — the daemon panicked; see daemon.log\n"}))));
                    });
                    continue;
                }
                "call" => Err(json!({"code": -32600, "message": "one call per connection"})),
                _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
            };
            if !id.is_null() || method.is_empty() {
                respond(&writer, id, reply);
            }
            // After its hello an MCP connection is an MCP session.
            if let Some(c) = caller.as_ref().filter(|c| c.mode == "mcp") {
                let _session = host.session();
                return mcp::serve_io(&mut reader, &writer, host, c.clone());
            }
        }
        // The client went away: a query is cancelled down to its server; a run goes on.
        cancel.cancel();
    });
}

fn respond(writer: &Mutex<UnixStream>, id: Value, reply: Result<Value, Value>) {
    let key = if reply.is_ok() { "result" } else { "error" };
    let _ = rpc::write(&mut *lock(writer), Framing::Lines, &json!({"jsonrpc": "2.0", "id": id, (key): reply.unwrap_or_else(|e| e)}));
}

/// Check the protocol and the caller; after a good hello the connection has no read timeout.
fn hello(host: &Host, params: Value, conn: &Mutex<UnixStream>) -> Result<(Caller, Value), Value> {
    if params["proto"].as_u64() != Some(proto()) {
        return Err(json!({"code": -32003, "message": format!("this daemon speaks proto {}", proto()), "data": {"daemon_proto": proto(), "daemon_version": VERSION}}));
    }
    match serde_json::from_value::<Caller>(params) {
        Ok(mut caller) if matches!(caller.mode.as_str(), "cli" | "hook" | "mcp") => {
            caller.id = host.usage.ident(&caller.mode, &caller.env);
            let reply = json!({"proto": proto(), "version": VERSION, "pid": process::id(), "root": host.ws.root, "ws": host.ws.ws, "started": sys::utc(host.started),
                "exe": [host.exe.0, host.exe.1], "busy": host.busy()});
            let _ = lock(conn).set_read_timeout(None);
            Ok((caller, reply))
        }
        other => Err(json!({"code": -32602, "message": format!("hello: {}", other.map_or_else(|e| e.to_string(), |c| format!("mode '{}' is not served", c.mode)))})),
    }
}

#[cfg(test)]
mod tests;
