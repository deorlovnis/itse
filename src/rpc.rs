//! JSON-RPC 2.0 framing and peers: one JSON value per line for MCP and the daemon
//! socket, `Content-Length` headers for LSP and DAP, and a `Peer` that matches requests with
//! responses over a child's stdio.
//!
//! One reader thread per peer. Responses go to the request waiting for them; requests from the
//! other side are answered at once by the `Handler`, so a server that asks something while we wait
//! on it can't deadlock us. Frames are capped at 4 MiB: a bigger one is read and thrown away, so the
//! stream stays in sync and the peer stays usable.

// The daemon socket uses the framing now; `Peer` is for the LSP, DAP and MCP clients, not built yet.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, BufRead, ErrorKind, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{mem, thread};

use serde_json::{Value, json};

use crate::sys::lock;

/// The frame cap of every peer and of the daemon socket.
pub const MAX_FRAME: usize = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Framing {
    /// One JSON value per line: MCP and the daemon socket.
    Lines,
    /// `Content-Length: N\r\n\r\n` before each body: LSP and DAP.
    Headers,
}

/// How ids are matched, requests cancelled and errors shaped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dialect {
    Lsp,
    Mcp,
    Dap,
}

/// The next message, or None at EOF before a frame starts. A frame over `max` is read to its end and
/// comes back as a failed response (`too_large`). A frame that isn't a JSON object (a child's stray
/// `1` is valid JSON) is an `InvalidData` error after which the stream is still in sync; after any
/// other error it isn't.
pub fn read(r: &mut impl BufRead, f: Framing, max: usize) -> io::Result<Option<Value>> {
    let body = match f {
        Framing::Lines => loop {
            let mut buf = Vec::new();
            match r.by_ref().take(max as u64 + 1).read_until(b'\n', &mut buf)? {
                0 => return Ok(None),
                // The rest of an oversized line goes through the reader's buffer, never into memory.
                n if n > max && buf.last() != Some(&b'\n') => return r.skip_until(b'\n').map(|_| Some(too_large(&buf))),
                _ if !buf.trim_ascii().is_empty() => break buf,
                _ => {}
            }
        },
        Framing::Headers => {
            let mut len = None;
            for n in 0.. {
                let mut line = String::new();
                match r.by_ref().take(4096).read_line(&mut line)? {
                    0 => return if n == 0 { Ok(None) } else { Err(ErrorKind::UnexpectedEof.into()) },
                    _ if line.trim().is_empty() => break,
                    _ => len = line.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok()).or(len),
                }
            }
            let len: usize = len.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "a header block without Content-Length"))?;
            // Of an oversized body only the head is kept, to find the id of the request it answers.
            let mut body = vec![0; if len > max { len.min(4096) } else { len }];
            r.read_exact(&mut body)?;
            if len > max {
                let rest = (len - body.len()) as u64;
                return if io::copy(&mut r.by_ref().take(rest), &mut io::sink())? < rest { Err(ErrorKind::UnexpectedEof.into()) } else { Ok(Some(too_large(&body))) };
            }
            body
        }
    };
    let what = |e: &serde_json::Error| if e.is_data() { "not a JSON object" } else { "not JSON" };
    serde_json::from_slice(&body).map(|m| Some(Value::Object(m))).map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("{}: {e}", what(&e))))
}

/// Write one frame and flush it.
pub fn write(w: &mut impl Write, f: Framing, v: &Value) -> io::Result<()> {
    let body = v.to_string();
    let head = if f == Framing::Headers { format!("Content-Length: {}\r\n\r\n", body.len()) } else { String::new() };
    w.write_all(format!("{head}{body}{}", if f == Framing::Lines { "\n" } else { "" }).as_bytes())?;
    w.flush()
}

/// A failed response, in JSON-RPC's and DAP's shape at once, to the id in an oversized frame's head
/// (its first `"request_seq"` or `"id"`), so the request it answered fails with `response_too_large`
/// instead of waiting for its timeout.
fn too_large(head: &[u8]) -> Value {
    let key = regex_lite::Regex::new(r#""(?:request_seq|id)"\s*:\s*(\d+)"#).expect("a valid pattern");
    let id: Option<i64> = key.captures(&String::from_utf8_lossy(&head[..head.len().min(4096)])).and_then(|c| c[1].parse().ok());
    json!({"id": id, "error": {"code": -32603, "message": "response_too_large"}, "type": "response", "request_seq": id, "success": false, "message": "response_too_large"})
}

/// What the other side sends us unasked. Calls run on the reader thread inside `catch_unwind`.
pub trait Handler: Send + Sync {
    fn notify(&self, _method: &str, _params: Value) {}
    /// A request from the other side; answered at once, so it never blocks.
    fn request(&self, method: &str, _params: Value) -> Result<Value, (i64, String)> {
        Err((-32601, format!("no {method}")))
    }
    fn closed(&self, _why: &str) {}
    /// A frame that isn't a JSON object, for the server's log.
    fn skipped(&self, _why: &str) {}
}

#[derive(Debug)]
pub enum RpcError {
    Timeout,
    Cancelled,
    Closed(String),
    Remote { code: i64, message: String, data: Option<Value> },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RpcError::Closed(why) => write!(f, "closed: {why}"),
            RpcError::Remote { code, message, .. } => write!(f, "{message} ({code})"),
            other => write!(f, "{}", format!("{other:?}").to_lowercase()),
        }
    }
}

impl std::error::Error for RpcError {}

/// Set once to cancel a request; shared by the caller and whoever waits for it. The second flag
/// says the caller asked (a Ctrl-C), not just went away: a run started by the call is cancelled
/// then, and left running when its caller only disconnects.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>, pub Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The requests in flight and, once the reader has ended, why.
type Pending = (HashMap<i64, mpsc::Sender<Result<Value, RpcError>>>, Option<String>);

/// One JSON-RPC (or DAP) connection, with its reader thread.
pub struct Peer {
    writer: Mutex<Box<dyn Write + Send>>,
    framing: Framing,
    dialect: Dialect,
    next: AtomicI64,
    /// The requests in flight and, once the reader has ended, why.
    state: Mutex<Pending>,
    /// DAP sends `cancel` only to adapters that declared `supportsCancelRequest`.
    pub dap_cancel: AtomicBool,
}

impl Peer {
    pub fn start(r: impl BufRead + Send + 'static, w: impl Write + Send + 'static, f: Framing, d: Dialect, h: Arc<dyn Handler>) -> Arc<Peer> {
        let peer = Arc::new(Peer { writer: Mutex::new(Box::new(w)), framing: f, dialect: d, next: AtomicI64::new(1), state: Mutex::default(), dap_cancel: AtomicBool::new(false) });
        let reader = peer.clone();
        thread::spawn(move || reader.read_loop(r, &*h));
        peer
    }

    /// Wait for the response in 100 ms steps, checking `cancel`. A timeout or a cancel sends the
    /// dialect's cancel and drops the entry, so a late response is ignored.
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &Cancel) -> Result<Value, RpcError> {
        let (id, (tx, rx)) = (self.next.fetch_add(1, Ordering::SeqCst), mpsc::channel());
        lock(&self.state).0.insert(id, tx);
        let (sent, end) = (self.send(&self.message(Some(id), method, params)), Instant::now() + timeout);
        let err = loop {
            match (sent.as_ref(), rx.recv_timeout(end.saturating_duration_since(Instant::now()).min(Duration::from_millis(100)))) {
                (_, Ok(reply)) => return reply,
                // Closed before this request was entered: nobody will answer it.
                _ if lock(&self.state).1.is_some() => break RpcError::Closed(lock(&self.state).1.clone().unwrap_or_default()),
                // A request that can't be written fails as closed, as the reader's EOF follows.
                (Err(e), _) => break RpcError::Closed(format!("write failed: {e}")),
                _ if cancel.is_cancelled() => break RpcError::Cancelled,
                _ if Instant::now() >= end => break RpcError::Timeout,
                _ => {}
            }
        };
        lock(&self.state).0.remove(&id);
        let _ = match self.dialect {
            _ if matches!(err, RpcError::Closed(_)) => Ok(()),
            Dialect::Lsp => self.notify("$/cancelRequest", json!({"id": id})),
            Dialect::Mcp => self.notify("notifications/cancelled", json!({"requestId": id, "reason": err.to_string()})),
            Dialect::Dap if self.dap_cancel.load(Ordering::SeqCst) => self.notify("cancel", json!({"requestId": id})),
            Dialect::Dap => Ok(()),
        };
        Err(err)
    }

    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.send(&self.message(None, method, params))
    }

    /// A request, or without an id a notification. DAP has no notifications: there it is a request
    /// whose response nobody waits for.
    fn message(&self, id: Option<i64>, method: &str, params: Value) -> Value {
        match (self.dialect, id) {
            (Dialect::Dap, _) => json!({"seq": id.unwrap_or_else(|| self.next.fetch_add(1, Ordering::SeqCst)), "type": "request", "command": method, "arguments": params}),
            (_, Some(id)) => json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            (_, None) => json!({"jsonrpc": "2.0", "method": method, "params": params}),
        }
    }

    fn send(&self, v: &Value) -> io::Result<()> {
        write(&mut *lock(&self.writer), self.framing, v)
    }

    /// Every request in flight when the reader ends, and every later one, fails as closed. A panic
    /// in a Handler call ends the peer as crashed instead of leaving it hung.
    fn read_loop(&self, mut r: impl BufRead, h: &dyn Handler) {
        let why = panic::catch_unwind(AssertUnwindSafe(|| self.serve(&mut r, h))).unwrap_or_else(|_| "crashed: handler panic".into());
        let pending = mem::replace(&mut *lock(&self.state), (HashMap::new(), Some(why.clone()))).0;
        pending.into_values().for_each(|tx| _ = tx.send(Err(RpcError::Closed(why.clone()))));
        h.closed(&why);
    }

    /// Read and dispatch until EOF or a read error; returns why it ended.
    fn serve(&self, r: &mut impl BufRead, h: &dyn Handler) -> String {
        loop {
            match read(r, self.framing, MAX_FRAME) {
                Ok(None) => return "eof".into(),
                Ok(Some(msg)) => self.dispatch(msg, h),
                Err(e) if e.kind() == ErrorKind::InvalidData => h.skipped(&e.to_string()),
                Err(e) => return format!("read failed: {e}"),
            }
        }
    }

    fn dispatch(&self, mut msg: Value, h: &dyn Handler) {
        // DAP in JSON-RPC's shape first, so one path serves all three dialects.
        if self.dialect == Dialect::Dap {
            msg = match msg["type"].as_str() {
                Some("response") if msg["success"] == true => json!({"id": msg["request_seq"], "result": msg["body"].take()}),
                Some("response") => json!({"id": msg["request_seq"], "error": {"code": 0, "message": msg["message"].as_str().unwrap_or("failed"), "data": msg["body"]}}),
                Some("event") => json!({"method": msg["event"], "params": msg["body"].take()}),
                _ => json!({"id": msg["seq"], "method": msg["command"], "params": msg["arguments"].take()}),
            };
        }
        match (msg["id"].take(), msg["method"].as_str().map(str::to_string)) {
            (Value::Null, Some(method)) => h.notify(&method, msg["params"].take()),
            // A response; one without an id (an oversized frame whose head held none) answers nothing.
            (id, None) => {
                let e = msg["error"].take();
                let remote = || RpcError::Remote { code: e["code"].as_i64().unwrap_or(0), message: e["message"].as_str().unwrap_or("").into(), data: e.get("data").cloned() };
                id.as_i64().and_then(|id| lock(&self.state).0.remove(&id)).map(|tx| tx.send(if e.is_null() { Ok(msg["result"].take()) } else { Err(remote()) }));
            }
            (id, Some(method)) => {
                let _ = self.send(&match (self.dialect, h.request(&method, msg["params"].take())) {
                    (Dialect::Dap, reply) => json!({"seq": self.next.fetch_add(1, Ordering::SeqCst), "type": "response", "request_seq": id, "command": method,
                        "success": reply.is_ok(), "body": reply.as_ref().ok(), "message": reply.as_ref().err().map(|e| &e.1)}),
                    (_, Ok(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    (_, Err((code, message))) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
