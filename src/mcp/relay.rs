//! `itse mcp`: the harness's stdio relayed line by line to the workspace daemon, as
//! Halti's `cred-shim mcp`, made line-aware. Through the relay, warm servers survive a harness
//! restart and the MCP session survives a daemon restart.
//!
//! The relay keeps the client's `initialize` and the requests in flight. When the daemon goes away,
//! it answers those (a `tools/call` with an `isError` result, others with -32603), connects again,
//! starting a daemon, replays `initialize` under its own id, swallows the reply and sends
//! `notifications/initialized`. Client lines that arrive meanwhile wait, up to 1 MB.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};
use std::{mem, thread};

use anyhow::Result;
use serde_json::{Value, json};

use crate::daemon::client;
use crate::results::text;
use crate::workspace::{Dirs, Workspace};

const REPLAY: &str = "itse-replay";
const HELD_MAX: usize = 1 << 20;

enum Msg {
    Client(Option<String>),
    /// A line from the daemon connection of one generation; None at its EOF.
    Daemon(u64, Option<String>),
}

/// Relay until the client closes stdin (exit 0); exit 1 when no daemon can be reached, after 3
/// losses in 60 s, or when more than 1 MB waits for a daemon.
pub fn run(ws: &Workspace, dirs: &Dirs) -> Result<u8> {
    let (tx, rx) = mpsc::channel();
    let client = tx.clone();
    thread::spawn(move || {
        io::stdin().lock().lines().map_while(Result::ok).try_for_each(|line| client.send(Msg::Client(Some(line)))).ok();
        let _ = client.send(Msg::Client(None));
    });
    let mut conn = match attach(ws, dirs, 0, &tx) {
        Ok(conn) => conn,
        Err(e) => return Ok(fail(&format!("{e:#}"))),
    };
    let (mut generation, mut init, mut flight, mut held, mut waiting, mut lost) = (0, None::<Value>, HashMap::new(), Vec::<String>::new(), false, Vec::<Instant>::new());
    let mut out = io::stdout().lock();
    for msg in rx {
        match msg {
            Msg::Client(None) => return Ok(0),
            Msg::Client(Some(line)) => {
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                if v["method"] == "initialize" {
                    init = Some(v.clone());
                }
                if !v["id"].is_null() && v["method"].is_string() {
                    flight.insert(v["id"].to_string(), (v["id"].clone(), (v["method"] == "tools/call").then(|| v["params"]["name"].as_str().unwrap_or("").to_string())));
                }
                // A failed write is followed by the EOF of that connection, which sends what is held.
                if waiting || conn.write_all(format!("{line}\n").as_bytes()).is_err() {
                    held.push(line);
                }
                if held.iter().map(String::len).sum::<usize>() > HELD_MAX {
                    return Ok(fail("more than 1 MB arrived while the daemon restarted"));
                }
            }
            Msg::Daemon(g, _) if g != generation => {}
            Msg::Daemon(_, Some(line)) => {
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                if v["id"] == REPLAY {
                    waiting = false;
                    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
                    for line in [initialized].into_iter().chain(held.drain(..)) {
                        let _ = conn.write_all(format!("{line}\n").as_bytes());
                    }
                    continue;
                }
                if v["method"].is_null() {
                    flight.remove(&v["id"].to_string());
                }
                if out.write_all(format!("{line}\n").as_bytes()).and_then(|()| out.flush()).is_err() {
                    return Ok(0);
                }
            }
            Msg::Daemon(_, None) => {
                // Requests still held never reached that daemon; they go to the next one.
                let unsent: Vec<String> = held.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|v| v["id"].to_string()).collect();
                flight.retain(|key, (id, tool)| {
                    let reply = match tool {
                        Some(tool) => json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("ERROR {tool} — {}", text("restarted", &[]))}], "isError": true}}),
                        None => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": text("restarted", &[])}}),
                    };
                    unsent.contains(key) || writeln!(out, "{reply}").and_then(|()| out.flush()).is_err()
                });
                lost.retain(|t| t.elapsed() < Duration::from_secs(60));
                lost.push(Instant::now());
                generation += 1;
                conn = loop {
                    match attach(ws, dirs, generation, &tx) {
                        Ok(conn) => break conn,
                        Err(e) if lost.len() >= 3 => return Ok(fail(&format!("the daemon went away 3 times in 60 s: {e:#}"))),
                        Err(_) => lost.push(Instant::now()),
                    }
                };
                let replay = init.as_ref().map(|i| json!({"jsonrpc": "2.0", "id": REPLAY, "method": "initialize", "params": i["params"]}).to_string());
                waiting = replay.is_some();
                for line in replay.into_iter().chain(if waiting { vec![] } else { mem::take(&mut held) }) {
                    let _ = conn.write_all(format!("{line}\n").as_bytes());
                }
            }
        }
    }
    Ok(0)
}

fn fail(why: &str) -> u8 {
    eprintln!("itse mcp: {why}");
    1
}

/// Connect in `mcp` mode, starting a daemon if none answers, and pass its lines to the main loop.
fn attach(ws: &Workspace, dirs: &Dirs, generation: u64, tx: &Sender<Msg>) -> Result<UnixStream> {
    let (reader, _) = client::connect(ws, dirs, "mcp", &[])?;
    let (tx, stream) = (tx.clone(), reader.get_ref().try_clone()?);
    thread::spawn(move || {
        reader.lines().map_while(Result::ok).try_for_each(|line| tx.send(Msg::Daemon(generation, Some(line)))).ok();
        let _ = tx.send(Msg::Daemon(generation, None));
    });
    Ok(stream)
}
