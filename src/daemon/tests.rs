use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::*;
use crate::sys::tests::TempDir;

fn host(tmp: &TempDir) -> Arc<Host> {
    Arc::new(unshared_host(tmp))
}

fn unshared_host(tmp: &TempDir) -> Host {
    let vars: BTreeMap<&str, OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let ws = Workspace::find(Some(tmp.0.clone()), &tmp.0).unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    Host::new(ws, dirs, socket, (1, 2))
}

/// A connection served on a thread, driven by hand.
struct Client {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Client {
    fn new(host: &Arc<Host>) -> Self {
        let (a, b) = UnixStream::pair().unwrap();
        let host = host.clone();
        thread::spawn(move || serve_conn(a, &host));
        b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self { r: BufReader::new(b.try_clone().unwrap()), w: b }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.w, "{line}").unwrap();
    }

    /// The next line, or Null at EOF or a reset (a close with our ping still unread).
    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let _ = self.r.read_line(&mut line);
        serde_json::from_str(&line).unwrap_or(Value::Null)
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string());
        self.recv()
    }

    fn hello(&mut self) -> Value {
        self.ask("hello", json!({"proto": PROTO, "version": VERSION, "mode": "cli", "cwd": "/", "env": {}}))
    }
}

#[test]
fn hello_is_required_first() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    assert_eq!(c.ask("call", json!({"verb": "status"}))["error"], json!({"code": -32002, "message": "hello required"}));
    assert_eq!(c.ask("ping", json!({}))["error"]["code"], -32002);
    let hello = c.hello();
    assert_eq!((hello["result"]["proto"].as_u64(), hello["result"]["exe"].clone()), (Some(PROTO), json!([1, 2])));
    assert_eq!(c.ask("ping", json!({}))["result"], json!({}));
    assert_eq!(c.ask("frobnicate", json!({}))["error"]["code"], -32601);
}

#[test]
fn a_proto_mismatch_is_32003_with_the_daemons_versions() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    let reply = c.ask("hello", json!({"proto": 99, "mode": "cli"}));
    assert_eq!(reply["error"]["code"], -32003);
    assert_eq!(reply["error"]["data"], json!({"daemon_proto": PROTO, "daemon_version": VERSION}));
    assert_eq!(c.ask("hello", json!({"proto": PROTO}))["error"]["code"], -32602, "a hello without a mode is refused");
}

#[test]
fn non_json_is_32700_and_the_connection_goes_on() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    c.send("this is not json");
    assert_eq!(c.recv()["error"]["code"], -32700);
    assert_eq!(c.hello()["result"]["ws"], json!(host(&tmp).ws.ws));
}

#[test]
fn json_that_is_no_object_is_32600_and_the_connection_goes_on() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    for line in ["5", "\"x\"", "[1,2]", "null"] {
        c.send(line);
        assert_eq!(c.recv()["error"]["code"], -32600, "{line}");
    }
    assert_eq!(c.hello()["result"]["ws"], json!(host(&tmp).ws.ws));
}

#[test]
fn a_call_answers_with_exit_and_text_and_a_second_call_is_refused() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    c.hello();
    let reply = c.ask("call", json!({"verb": "status", "args": {"target": null, "rest": [], "flags": {}}}));
    assert_eq!(reply["result"]["exit"], 0);
    assert!(reply["result"]["text"].as_str().unwrap().starts_with(&format!("OK status {} — daemon pid ", tmp.0.display())));
    assert_eq!(c.ask("call", json!({"verb": "status"}))["error"]["code"], -32600);
}

#[test]
fn mcp_args_go_through_the_cli_parser() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    c.hello();
    let reply = c.ask("call", json!({"verb": "status", "args": ["--frobnicate"]}));
    assert_eq!(reply["result"], json!({"exit": 2, "text": "ERROR status — invalid option '--frobnicate'\n"}));
}

#[test]
fn args_that_are_not_strings_are_refused() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    c.hello();
    let reply = c.ask("call", json!({"verb": "status", "args": [14]}));
    assert_eq!(reply["result"], json!({"exit": 2, "text": "ERROR status — args must be strings\n"}));
}

#[test]
fn a_cancel_request_ends_a_slow_call() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    c.ask("hello", json!({"proto": PROTO, "mode": "cli", "env": {"ITSE_TEST_SLOW_MS": "30000"}}));
    let started = Instant::now();
    c.send(&json!({"jsonrpc": "2.0", "id": 7, "method": "call", "params": {"verb": "status"}}).to_string());
    thread::sleep(Duration::from_millis(100));
    c.send(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 7}}).to_string());
    assert_eq!(c.recv(), json!({"jsonrpc": "2.0", "id": 7, "result": {"exit": 130, "text": "ERROR status — cancelled\n"}}));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_silent_connection_is_closed_after_the_hello_wait_and_counted_until_then() {
    let tmp = TempDir::new("itse-daemon");
    let mut host = unshared_host(&tmp);
    host.idle = Duration::ZERO;
    let (host, (a, _b)) = (Arc::new(host), UnixStream::pair().unwrap());
    let (started, conn) = (Instant::now(), host.clone());
    let served = thread::spawn(move || serve_conn(a, &conn));
    thread::sleep(Duration::from_millis(200));
    assert!(!host.idle(), "an open connection keeps the daemon busy");
    served.join().unwrap();
    assert!(started.elapsed() >= HELLO_WAIT && started.elapsed() < HELLO_WAIT + Duration::from_secs(2));
    assert!(host.idle(), "a closed one doesn't");
}

#[test]
fn a_connection_that_talks_but_never_says_hello_is_closed_after_5_s() {
    let tmp = TempDir::new("itse-daemon");
    let mut c = Client::new(&host(&tmp));
    let started = Instant::now();
    while started.elapsed() < HELLO_WAIT + Duration::from_secs(3) {
        if writeln!(c.w, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).is_err() || c.recv().is_null() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    assert!(started.elapsed() >= HELLO_WAIT && started.elapsed() < HELLO_WAIT + Duration::from_secs(2), "{:?}", started.elapsed());
}
