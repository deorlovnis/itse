use std::io::{BufReader, Cursor};
use std::os::unix::net::UnixStream;

use super::*;

/// A reader that hands out one byte per read, the worst a pipe can do.
struct Trickle(Cursor<Vec<u8>>);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(1);
        self.0.read(&mut buf[..n])
    }
}

fn read_all(bytes: &[u8], f: Framing, max: usize) -> Vec<io::Result<Option<Value>>> {
    let mut r = BufReader::with_capacity(1, Trickle(Cursor::new(bytes.to_vec())));
    let mut out = Vec::new();
    loop {
        let m = read(&mut r, f, max);
        let end = matches!(m, Ok(None)) || matches!(&m, Err(e) if e.kind() != ErrorKind::InvalidData);
        out.push(m);
        if end || out.len() > 10 {
            return out;
        }
    }
}

fn ok(m: &io::Result<Option<Value>>) -> &Value {
    m.as_ref().unwrap().as_ref().unwrap()
}

#[test]
fn both_framings_round_trip() {
    for f in [Framing::Lines, Framing::Headers] {
        let mut buf = Vec::new();
        let (a, b) = (json!({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {"s": "é\n"}}), json!({"id": 1, "result": [1, 2]}));
        write(&mut buf, f, &a).unwrap();
        write(&mut buf, f, &b).unwrap();
        let got = read_all(&buf, f, MAX_FRAME);
        assert_eq!((ok(&got[0]), ok(&got[1])), (&a, &b), "{f:?}");
        assert!(matches!(got[2], Ok(None)));
    }
}

#[test]
fn mixed_case_content_length_is_read_and_content_type_ignored() {
    let body = r#"{"id":7}"#;
    let text = format!("content-LENGTH: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{body}", body.len());
    assert_eq!(ok(&read_all(text.as_bytes(), Framing::Headers, MAX_FRAME)[0]), &json!({"id": 7}));
    let err = read_all(b"Content-Type: x\r\n\r\n{}", Framing::Headers, MAX_FRAME).remove(0).unwrap_err();
    assert_eq!(err.to_string(), "a header block without Content-Length");
}

#[test]
fn truncated_bodies_and_headers_are_eof_errors() {
    for text in ["Content-Length: 10\r\n\r\n{\"a\":", "Content-Length: 10\r\n"] {
        let err = read_all(text.as_bytes(), Framing::Headers, MAX_FRAME).remove(0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "{text:?}");
    }
    let err = read_all(b"Content-Length: 100\r\n\r\n{\"id\":3,", Framing::Headers, 10).remove(0).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "an oversized body cut short is EOF too");
}

#[test]
fn blank_and_non_json_lines_are_skipped_or_reported_and_the_stream_stays_in_sync() {
    let got = read_all(b"\n  \nnot json\n{\"id\":1}\n", Framing::Lines, MAX_FRAME);
    assert!(got[0].as_ref().unwrap_err().to_string().starts_with("not JSON: "));
    assert_eq!(ok(&got[1]), &json!({"id": 1}));
}

#[test]
fn json_that_is_no_object_is_reported_and_the_stream_stays_in_sync() {
    for f in [Framing::Lines, Framing::Headers] {
        let mut buf = Vec::new();
        for v in [json!(5), json!("x"), json!([1, 2]), json!(null), json!({"id": 1})] {
            write(&mut buf, f, &v).unwrap();
        }
        let got = read_all(&buf, f, MAX_FRAME);
        assert!(got[..4].iter().all(|m| m.as_ref().unwrap_err().to_string().starts_with("not a JSON object: ")), "{f:?}: {got:?}");
        assert_eq!(ok(&got[4]), &json!({"id": 1}), "{f:?}");
    }
}

#[test]
fn an_oversized_frame_becomes_a_failed_response_to_its_id_in_both_framings() {
    let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":\"{}\"}}", "x".repeat(100));
    let lines = format!("{big}\n{{\"id\":2}}\n");
    let headers = format!("Content-Length: {}\r\n\r\n{big}Content-Length: 8\r\n\r\n{{\"id\":2}}", big.len());
    for (f, text) in [(Framing::Lines, lines), (Framing::Headers, headers)] {
        let got = read_all(text.as_bytes(), f, 64);
        assert_eq!((&ok(&got[0])["id"], &ok(&got[0])["error"]["message"]), (&json!(42), &json!("response_too_large")), "{f:?}");
        assert_eq!(ok(&got[1]), &json!({"id": 2}), "{f:?}");
    }
}

/// What a peer's Handler saw.
#[derive(Default)]
struct Rec {
    notes: Mutex<Vec<(String, Value)>>,
    skipped: Mutex<Vec<String>>,
    closed: Mutex<Option<String>>,
    panics: bool,
}

impl Handler for Rec {
    fn notify(&self, method: &str, params: Value) {
        assert!(!self.panics, "handler panic on purpose");
        lock(&self.notes).push((method.into(), params));
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        match method {
            "workspace/configuration" => Ok(json!([{"asked": params}])),
            _ => Err((-32601, format!("no {method}"))),
        }
    }

    fn closed(&self, why: &str) {
        *lock(&self.closed) = Some(why.into());
    }

    fn skipped(&self, why: &str) {
        lock(&self.skipped).push(why.into());
    }
}

/// The far end of a peer, speaking JSON by hand rather than through this module's codec.
struct Server {
    r: BufReader<UnixStream>,
    w: UnixStream,
    f: Framing,
}

impl Server {
    fn recv(&mut self) -> Value {
        let mut line = String::new();
        if self.f == Framing::Lines {
            self.r.read_line(&mut line).unwrap();
            return serde_json::from_str(&line).unwrap();
        }
        let mut len = 0;
        loop {
            line.clear();
            self.r.read_line(&mut line).unwrap();
            match line.trim().strip_prefix("Content-Length: ") {
                Some(n) => len = n.parse().unwrap(),
                None if line.trim().is_empty() => break,
                None => {}
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn send_raw(&mut self, body: &str) {
        let frame = match self.f {
            Framing::Lines => format!("{body}\n"),
            Framing::Headers => format!("Content-Length: {}\r\n\r\n{body}", body.len()),
        };
        self.w.write_all(frame.as_bytes()).unwrap();
    }

    fn send(&mut self, v: Value) {
        self.send_raw(&v.to_string());
    }
}

fn pair(f: Framing, h: Arc<Rec>) -> (Arc<Peer>, Server) {
    let (a, b) = UnixStream::pair().unwrap();
    let peer = Peer::start(BufReader::new(a.try_clone().unwrap()), a, f, h);
    (peer, Server { r: BufReader::new(b.try_clone().unwrap()), w: b, f })
}

const LONG: Duration = Duration::from_secs(10);

/// Run a request on another thread, so the test can play the server meanwhile.
fn spawn_request(peer: &Arc<Peer>, method: &'static str, timeout: Duration, cancel: Cancel) -> thread::JoinHandle<Result<Value, RpcError>> {
    let peer = peer.clone();
    thread::spawn(move || peer.request(method, json!({}), timeout, &cancel))
}

#[test]
fn out_of_order_responses_reach_their_requests() {
    let (peer, mut srv) = pair(Framing::Headers, Arc::default());
    let a = spawn_request(&peer, "a", LONG, Cancel::default());
    let ida = srv.recv()["id"].clone();
    let b = spawn_request(&peer, "b", LONG, Cancel::default());
    let idb = srv.recv()["id"].clone();
    srv.send(json!({"jsonrpc": "2.0", "id": idb, "result": "for b"}));
    srv.send(json!({"jsonrpc": "2.0", "id": ida, "error": {"code": -32800, "message": "gone", "data": 1}}));
    assert_eq!(b.join().unwrap().unwrap(), "for b");
    assert!(matches!(a.join().unwrap(), Err(RpcError::Remote { code: -32800, message }) if message == "gone"));
}

#[test]
fn an_oversized_body_is_drained_and_the_peer_stays_usable() {
    let (peer, mut srv) = pair(Framing::Headers, Arc::default());
    let a = spawn_request(&peer, "a", LONG, Cancel::default());
    let id = srv.recv()["id"].clone();
    srv.send(json!({"jsonrpc": "2.0", "id": id, "result": "x".repeat(MAX_FRAME + 10)}));
    assert!(matches!(a.join().unwrap(), Err(RpcError::Remote { message, .. }) if message == "response_too_large"));
    let b = spawn_request(&peer, "b", LONG, Cancel::default());
    let id = srv.recv()["id"].clone();
    srv.send(json!({"jsonrpc": "2.0", "id": id, "result": 2}));
    assert_eq!(b.join().unwrap().unwrap(), 2);
}

#[test]
fn non_json_lines_from_a_child_are_skipped_and_logged() {
    let rec = Arc::new(Rec::default());
    let (peer, mut srv) = pair(Framing::Lines, rec.clone());
    let a = spawn_request(&peer, "workspace/symbol", LONG, Cancel::default());
    let id = srv.recv()["id"].clone();
    srv.send_raw("Starting server on stdout...");
    srv.send(json!({"jsonrpc": "2.0", "id": id, "result": []}));
    assert_eq!(a.join().unwrap().unwrap(), json!([]));
    assert_eq!(lock(&rec.skipped).len(), 1);
}

#[test]
fn json_lines_that_are_no_object_are_skipped_and_the_peer_stays_usable() {
    let rec = Arc::new(Rec::default());
    let (peer, mut srv) = pair(Framing::Lines, rec.clone());
    let a = spawn_request(&peer, "x", LONG, Cancel::default());
    let req = srv.recv();
    ["1", "\"x\"", "[\"x\"]", "null"].into_iter().for_each(|line| srv.send_raw(line));
    srv.send(json!({"jsonrpc": "2.0", "id": req["id"], "result": 2}));
    assert_eq!(a.join().unwrap().unwrap(), 2);
    assert_eq!((lock(&rec.skipped).len(), lock(&rec.closed).clone()), (4, None));
}

#[test]
fn a_timeout_sends_cancel_request() {
    let (peer, mut srv) = pair(Framing::Headers, Arc::default());
    let started = Instant::now();
    let a = spawn_request(&peer, "slow", Duration::from_millis(200), Cancel::default());
    let id = srv.recv()["id"].clone();
    assert!(matches!(a.join().unwrap(), Err(RpcError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(srv.recv(), json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}));
}

#[test]
fn a_cancel_sends_cancel_request() {
    let (peer, mut srv) = pair(Framing::Headers, Arc::default());
    let cancel = Cancel::default();
    let a = spawn_request(&peer, "workspace/symbol", LONG, cancel.clone());
    let id = srv.recv()["id"].clone();
    cancel.cancel();
    assert!(matches!(a.join().unwrap(), Err(RpcError::Cancelled)));
    assert_eq!(srv.recv(), json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}));
}

#[test]
fn a_server_request_is_answered_while_a_client_request_waits() {
    let rec = Arc::new(Rec::default());
    let (peer, mut srv) = pair(Framing::Headers, rec.clone());
    let a = spawn_request(&peer, "initialize", LONG, Cancel::default());
    let id = srv.recv()["id"].clone();
    srv.send(json!({"jsonrpc": "2.0", "id": "s1", "method": "workspace/configuration", "params": {"items": []}}));
    assert_eq!(srv.recv(), json!({"jsonrpc": "2.0", "id": "s1", "result": [{"asked": {"items": []}}]}));
    srv.send(json!({"jsonrpc": "2.0", "id": 7, "method": "window/unknown"}));
    assert_eq!(srv.recv()["error"], json!({"code": -32601, "message": "no window/unknown"}));
    srv.send(json!({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": 1}}));
    srv.send(json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}));
    assert_eq!(a.join().unwrap().unwrap(), json!({"capabilities": {}}));
    assert_eq!(lock(&rec.notes).as_slice(), &[("$/progress".to_string(), json!({"token": 1}))]);
}

#[test]
fn eof_fails_pending_requests_and_later_ones() {
    let rec = Arc::new(Rec::default());
    let (peer, mut srv) = pair(Framing::Headers, rec.clone());
    let a = spawn_request(&peer, "a", LONG, Cancel::default());
    srv.recv();
    drop(srv);
    assert!(matches!(a.join().unwrap(), Err(RpcError::Closed(why)) if why == "eof"));
    assert_eq!(lock(&rec.closed).as_deref(), Some("eof"));
    assert!(matches!(peer.request("b", json!({}), LONG, &Cancel::default()), Err(RpcError::Closed(_))));
}

#[test]
fn a_reply_that_came_just_before_the_close_is_taken_not_dropped() {
    let (peer, mut srv) = pair(Framing::Headers, Arc::default());
    let a = spawn_request(&peer, "a", LONG, Cancel::default());
    let id = srv.recv()["id"].as_i64().unwrap();
    // Held across the end of a 100 ms step, the caller waits on the state there. Meanwhile the
    // reply and then the close come, in the reader's order, as from a server that answers and exits.
    let mut state = lock(&peer.state);
    thread::sleep(Duration::from_millis(250));
    state.0.remove(&id).unwrap().send(Ok(json!("answered"))).unwrap();
    state.1 = Some("eof".into());
    drop(state);
    assert_eq!(a.join().unwrap().unwrap(), "answered");
}

#[test]
fn a_panicking_handler_marks_the_peer_crashed() {
    let rec = Arc::new(Rec { panics: true, ..Rec::default() });
    let (peer, mut srv) = pair(Framing::Lines, rec.clone());
    let a = spawn_request(&peer, "a", LONG, Cancel::default());
    srv.recv();
    srv.send(json!({"jsonrpc": "2.0", "method": "boom"}));
    assert!(matches!(a.join().unwrap(), Err(RpcError::Closed(why)) if why == "crashed: handler panic"));
    assert_eq!(lock(&rec.closed).as_deref(), Some("crashed: handler panic"));
    assert!(matches!(peer.request("b", json!({}), LONG, &Cancel::default()), Err(RpcError::Closed(_))));
}

#[test]
fn a_peer_that_stops_reading_blocks_no_caller() {
    let (peer, srv) = pair(Framing::Headers, Arc::default());
    // Far more than the socket buffer holds, and nobody reads it.
    peer.notify("textDocument/didChange", json!({"text": "x".repeat(8 << 20)})).unwrap();
    let started = Instant::now();
    assert!(matches!(peer.request("workspace/symbol", json!({}), Duration::from_millis(200), &Cancel::default()), Err(RpcError::Timeout)));
    assert!(peer.bye(Duration::from_millis(200)));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    drop(srv);
}
