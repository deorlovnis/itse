//! The CLI's side of the daemon socket: connect, starting the daemon
//! when none answers; `hello` with a 5 s timeout and the protocol and binary checks; one call, with
//! Ctrl-C turned into a cancel. The `itse mcp` relay connects through here too.

use std::io::{BufReader, ErrorKind};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{HELLO_WAIT, VERSION, exe_id, proto};
use crate::rpc::{self, Framing, RpcError};
use crate::sys;
use crate::workspace::{Dirs, Workspace};

/// A connection to the daemon after a good `hello`, and the hello's reply.
pub type Conn = (BufReader<UnixStream>, Value);

/// The one request in flight on a connection (`hello`, `call`, or `shutdown` to a daemon that doesn't
/// match), as id 0, answered within `wait` (None waits as long as it takes).
pub fn ask(reader: &mut BufReader<UnixStream>, method: &str, params: Value, wait: Option<Duration>) -> Result<Value, RpcError> {
    let closed = |e: std::io::Error| RpcError::Closed(e.to_string());
    reader.get_ref().set_read_timeout(wait).map_err(closed)?;
    rpc::write(&mut reader.get_ref(), Framing::Lines, &json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params})).map_err(closed)?;
    let reply = rpc::read(reader, Framing::Lines, rpc::MAX_FRAME);
    let _ = reader.get_ref().set_read_timeout(None);
    match reply {
        Ok(Some(r)) if r["error"].is_object() => {
            Err(RpcError::Remote { code: r["error"]["code"].as_i64().unwrap_or(0), message: r["error"]["message"].as_str().unwrap_or("").into() })
        }
        Ok(Some(mut r)) => Ok(r["result"].take()),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Err(RpcError::Timeout),
        Err(e) => Err(closed(e)),
        Ok(None) => Err(RpcError::Closed("eof".into())),
    }
}

/// Connect to the workspace's daemon and say hello in `mode` (`cli`, `mcp` or `hook`; the first two
/// also name who started a daemon), with this process's env and `extra` over it. A daemon is started
/// when none answers, and replaced when it speaks another protocol or, idle, runs a replaced binary.
/// The relay never replaces one for its binary: MCP lines pass through any daemon of its protocol,
/// and an upgrade would shut down the daemon a newer CLI just started. A hook never starts or
/// replaces one: without a daemon of its protocol it fails.
pub fn connect(ws: &Workspace, dirs: &Dirs, mode: &str, extra: &[(&str, &str)]) -> Result<Conn> {
    let (socket, exe, hook) = (ws.socket(dirs, Path::new("/tmp"))?, exe_id(), mode == "hook");
    let mut env: serde_json::Map<String, Value> = env::vars_os().map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into())).collect();
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), json!(v))));
    let (exe_path, cwd) = (env::current_exe().unwrap_or_default(), env::current_dir().unwrap_or_default());
    let hello = json!({"proto": proto(), "version": VERSION, "exe": [exe.0, exe.1], "exe_path": exe_path, "mode": mode, "cwd": cwd, "env": env});
    for last in [false, false, true] {
        let stream = match UnixStream::connect(&socket) {
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) && !hook => start(ws, dirs, mode, &socket)?,
            stream => stream.with_context(|| format!("connecting to {}", socket.display()))?,
        };
        // The hello carries the env: only to a daemon of ours.
        let peer = sys::peer_uid(&stream).with_context(|| format!("asking who serves {}", socket.display()))?;
        ensure!(peer == sys::uid(), "{} is served by uid {peer}, not by us; no hello is sent", socket.display());
        let mut reader = BufReader::new(stream);
        let reason = match ask(&mut reader, "hello", hello.clone(), Some(HELLO_WAIT)) {
            Err(RpcError::Remote { code: -32003, .. }) => "proto",
            Ok(reply) if mode == "cli" && reply["exe"] != json!([exe.0, exe.1]) && reply["busy"] == false => "upgrade",
            Ok(hello) => return Ok((reader, hello)),
            Err(RpcError::Timeout) => {
                bail!("the daemon at {} did not answer hello in 5 s (pid {}); 'itse daemon stop' kills it", socket.display(), running(ws, dirs).map_or(0, |d| d.0))
            }
            // EOF or a reset: that daemon was stopping and dropped the queued connection.
            Err(RpcError::Closed(_)) if !last => continue,
            Err(e) => bail!("hello: {e}"),
        };
        ensure!(!hook, "the daemon speaks another protocol");
        if !last {
            let _ = ask(&mut reader, "shutdown", json!({"reason": reason}), Some(HELLO_WAIT));
        }
    }
    bail!("the daemon at {} was replaced twice and still doesn't match this itse", socket.display())
}

/// Run `__daemonize`, rotating `daemon.log` past 4 MB first, and wait for the socket: every 20 ms
/// for 10 s, longer than a stopping daemon's 3 s cap.
fn start(ws: &Workspace, dirs: &Dirs, by: &str, socket: &Path) -> Result<UnixStream> {
    let (state, end) = (ws.state_dir(dirs), Instant::now() + Duration::from_secs(10));
    let log = state.join("daemon.log");
    sys::private_dir(&state)?;
    if fs::metadata(&log).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = fs::rename(&log, state.join("daemon.log.1"));
    }
    let mut cmd = Command::new("/proc/self/exe");
    let started = cmd.arg("--root").arg(&ws.root).arg("__daemonize").env("ITSE_STARTED_BY", by).stdin(Stdio::null()).status().context("starting the daemon")?;
    while started.success() && Instant::now() < end {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    let text = fs::read_to_string(&log).unwrap_or_default();
    bail!("the daemon didn't start (see {})\n{}", log.display(), text.lines().skip(text.lines().count().saturating_sub(3)).collect::<Vec<_>>().join("\n"))
}

/// Send the one `call` and wait for its reply: `(text, exit)`. The first Ctrl-C asks the daemon to
/// cancel the call, and it answers; the second exits 130 at once.
pub fn call((mut reader, _): Conn, params: Value) -> Result<(String, u8)> {
    sys::catch_signals();
    let mut watcher = reader.get_ref().try_clone()?;
    thread::spawn(move || {
        for asked in [false, true] {
            while !sys::signalled() {
                thread::sleep(Duration::from_millis(100));
            }
            let _ = if asked { process::exit(130) } else { rpc::write(&mut watcher, Framing::Lines, &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 0}})) };
        }
    });
    let reply = ask(&mut reader, "call", params, None)?;
    Ok((reply["text"].as_str().unwrap_or("").to_string(), reply["exit"].as_u64().map_or(2, |e| e.min(255) as u8)))
}

/// The daemon `daemon.json` names, if that process still runs: `(pid, start_time, daemon.json)`.
pub fn running(ws: &Workspace, dirs: &Dirs) -> Option<(i32, u64, Value)> {
    let info: Value = fs::read(ws.state_dir(dirs).join("daemon.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
    info["pid"].as_i64().zip(info["start_time"].as_u64()).map(|(p, s)| (p as i32, s, info.clone())).filter(|(p, s, _)| sys::alive(*p, *s))
}
