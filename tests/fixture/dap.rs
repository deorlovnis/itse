//! `itse-fixture dap [--tcp PORT]`: a debug adapter on stdio, or on one connection to
//! 127.0.0.1:PORT (the fixture listens; Itse only connects). The debuggee is make-believe: one
//! thread `main` whose stack mixes project frames (`src/lib.rs` under the root, which is the
//! launch's `cwd` or the fixture's own folder; one by path, one by `file://` URI) with framework
//! ones (outside the root, under `target/`, without a source). It stops at the first line
//! breakpoint, else with caught or uncaught exception filters on an exception, else it ends.
//! Logpoints print their text as `output`; a breakpoint past the source's last line, or in no
//! source, is unverified. `continue` ends it, `next`, `stepIn` and `stepOut`
//! stop one line down.
//!
//! Locals: `order` (with children), `this` (30 children, one of them nested without end), a long
//! string, `$i$a$secret` (for `hide_vars`), `$exception` at an exception, and `locals` more from
//! `ITSE_FIXTURE_SCRIPT` (`{"locals": 100}`); an expensive `Registers` scope that must never be
//! read. Every message it gets is appended to `ITSE_FIXTURE_LOG`. Modes: `java-order` sends
//! `initialized` only after `launch` or `attach` and answers that only after `configurationDone`
//! (never, with the script's `hold_launch`); `caught-only` offers only a plain `caught` filter and no logpoints, and
//! first stops twice in a framework frame, as `framework-first` does with a caught filter that
//! takes a condition; `launch-fails` refuses `launch` and sends no `initialized`; `eof-on-continue`
//! and `eof-on-scopes` end the connection when asked to continue or for scopes; `no-thread-id`
//! names its one thread 7 and leaves `threadId` out of `stopped`; `spawn-child` starts a `sleep` in
//! a process group of its own at `initialize` and writes its pid to the log's path plus `.child`. The
//! script's `reject_conditions` refuses exception filters with a condition, `break_mode` is what
//! `exceptionInfo` answers (`always`), `fail_exception_info` refuses it, and `long_names` pads names (a frame's, the thread's and the
//! extra locals', and their values) to that many chars. It sends a `telemetry` output, and asks Itse once to
//! `runInTerminal`, which Itse refuses. `launch` and `attach` refuse arguments that are no object,
//! as strict adapters do. `double-stop` sends a stop of thread 2 alone too (as adapters with a stop
//! per thread do), after a breakpoint's stop and before a step's; `chatty` writes 6 MiB to stderr at
//! `initialize`; `never-stops` neither stops nor ends; the script's `stop_after_ms` holds the first
//! stop that long.

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::{env, path::PathBuf};

use serde_json::{Value, json};

use crate::lsp::frame;

/// A stack frame at `line` of `path` (none without one).
fn stack_frame(id: u64, name: &str, path: Value, line: u64) -> Value {
    json!({"id": id, "name": name, "source": if path.is_null() { Value::Null } else { json!({"path": path}) }, "line": line, "column": 5})
}

struct Adapter<W: Write> {
    out: W,
    seq: u64,
    mode: String,
    root: PathBuf,
    /// The first line breakpoint, by path and line; the logpoints' texts.
    stop_at: Option<(String, u64)>,
    logs: Vec<String>,
    exceptions: bool,
    /// Stops left before the debuggee ends: framework stops first in `caught-only`.
    stops: Vec<bool>,
    line: u64,
    launched: Option<Value>,
    configured: bool,
    script: Value,
}

impl<W: Write> Adapter<W> {
    fn send(&mut self, mut v: Value) {
        self.seq += 1;
        v["seq"] = self.seq.into();
        let body = v.to_string();
        let _ = write!(self.out, "Content-Length: {}\r\n\r\n{body}", body.len()).and_then(|()| self.out.flush());
    }

    fn event(&mut self, event: &str, body: Value) {
        self.send(json!({"type": "event", "event": event, "body": body}));
    }

    fn reply(&mut self, req: &Value, body: Value) {
        self.send(json!({"type": "response", "request_seq": req["seq"], "command": req["command"], "success": true, "body": body}));
    }

    /// Once launched and configured: answer a held `launch`, print the logpoints, and stop or end.
    fn go(&mut self) {
        let Some(launch) = self.launched.take_if(|_| self.configured) else { return };
        if self.mode == "java-order" {
            if self.script["hold_launch"] == true {
                return;
            }
            self.reply(&launch, json!({}));
        }
        self.event("output", json!({"category": "telemetry", "output": "{\"telemetry\": true}\n"}));
        if self.mode == "never-stops" {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(self.script["stop_after_ms"].as_u64().unwrap_or(0)));
        for text in self.logs.clone() {
            self.event("output", json!({"category": "console", "output": format!("{text}\n")}));
        }
        self.stops = match (&self.stop_at, self.exceptions) {
            (Some(_), _) => vec![true],
            (None, true) if self.mode == "caught-only" || self.mode == "framework-first" => vec![false, false, true],
            (None, true) => vec![true],
            _ => vec![],
        };
        self.next_stop();
    }

    fn next_stop(&mut self) {
        if self.stops.is_empty() {
            self.event("output", json!({"category": "stdout", "output": "test finished\n"}));
            self.event("exited", json!({"exitCode": 0}));
            return self.event("terminated", json!({}));
        }
        let reason = if self.stop_at.is_some() { "breakpoint" } else { "exception" };
        self.stopped(reason);
    }

    fn thread(&self) -> u64 {
        if self.mode == "no-thread-id" { 7 } else { 1 }
    }

    fn stopped(&mut self, reason: &str) {
        let mut body = json!({"reason": reason, "allThreadsStopped": true});
        if self.mode != "no-thread-id" {
            body["threadId"] = 1.into();
        }
        let other = json!({"reason": "breakpoint", "threadId": 2, "allThreadsStopped": false});
        if self.mode == "double-stop" && reason == "step" {
            self.event("stopped", other.clone());
        }
        self.event("stopped", body);
        if self.mode == "double-stop" && reason != "step" {
            self.event("stopped", other);
        }
    }

    /// `name`, padded to the script's `long_names` chars.
    fn long(&self, name: &str) -> String {
        format!("{name}{}", "_".repeat((self.script["long_names"].as_u64().unwrap_or(0) as usize).saturating_sub(name.len())))
    }

    fn frames(&self) -> Vec<Value> {
        let root = self.root.display();
        let (path, line) = self.stop_at.clone().unwrap_or_else(|| (format!("{root}/src/lib.rs"), 2));
        let mut frames = vec![];
        if self.stops.first() == Some(&false) {
            frames.push(stack_frame(1, "Framework.dispatch", json!("/usr/lib/jvm/Framework.java"), 10));
        }
        frames.push(stack_frame(2, &self.long("orders::save"), json!(path), self.line.max(line)));
        frames.push(stack_frame(3, "orders::tests::saves", json!(format!("file://{root}/src/lib.rs")), 1));
        frames.push(stack_frame(4, "core::panicking::panic", json!("/rustc/abc/library/core/src/panicking.rs"), 50));
        frames.push(stack_frame(5, "build_helper", json!(format!("{root}/target/debug/build/x.rs")), 1));
        frames.push(stack_frame(6, "<no source>", Value::Null, 0));
        frames
    }

    fn variables(&self, reference: u64) -> Vec<Value> {
        let v = |name: &str, value: &str, reference: u64| json!({"name": name, "type": "T", "value": value, "variablesReference": reference});
        match reference {
            1000..2000 => {
                let mut locals =
                    vec![v("order", "Order(id=O-17)", 2000), v("this", "OrderRepository@1", 3000), v("$i$a$secret", "hidden-value", 0), v("long", &"x".repeat(500), 0)];
                locals.extend(self.stop_at.is_none().then(|| v("$exception", "AssertionError", 5000)));
                locals.extend((0..self.script["locals"].as_u64().unwrap_or(0)).map(|i| v(&self.long(&format!("v{i}")), &self.long(&i.to_string()), 0)));
                locals
            }
            2000 => vec![v("id", "\"O-17\"", 0), v("lines", "3", 0)],
            3000 => [v("jdbc", "JdbcTemplate@4f2a", 3001)].into_iter().chain((0..30).map(|i| v(&format!("c{i}"), "child", 0))).collect(),
            3001..4000 => vec![v(&format!("level{}", reference - 3000), "deeper", reference + 1)],
            5000 => vec![v("message", "expected 2, got 3", 0)],
            _ => vec![v("never", "read", 0)],
        }
    }

    /// One request; false once the connection should end.
    fn handle(&mut self, req: &Value) -> bool {
        let a = &req["arguments"];
        match req["command"].as_str().unwrap_or("") {
            "initialize" => {
                let caught =
                    if self.mode == "caught-only" { json!({"filter": "caught", "label": "Caught Exceptions"}) } else { json!({"filter": "caught", "supportsCondition": true}) };
                if self.mode == "chatty" {
                    let _ = io::stderr().write_all(&vec![b'x'; 6 << 20]);
                }
                if self.mode == "spawn-child" {
                    let child = Command::new("sleep").arg("300").process_group(0).spawn().map(|c| c.id().to_string()).unwrap_or_default();
                    let _ = std::fs::write(format!("{}.child", env::var("ITSE_FIXTURE_LOG").unwrap_or_default()), child);
                }
                let mut filters = vec![caught];
                filters.extend((self.mode != "caught-only").then(|| json!({"filter": "uncaught"})));
                let caps = json!({"supportsConfigurationDoneRequest": true, "supportsLogPoints": self.mode != "caught-only", "supportsExceptionInfoRequest": true,
                    "supportsExceptionFilterOptions": self.mode != "caught-only", "exceptionBreakpointFilters": filters});
                self.reply(req, caps);
                if self.mode != "java-order" && self.mode != "launch-fails" {
                    self.event("initialized", json!({}));
                }
            }
            "launch" if self.mode == "launch-fails" => self.fail(req, "program not found: nope"),
            "launch" | "attach" if !a.is_object() => self.fail(req, "arguments must be an object"),
            "launch" | "attach" => {
                self.root = a["cwd"].as_str().map_or(self.root.clone(), PathBuf::from);
                self.send(json!({"type": "request", "command": "runInTerminal", "arguments": {"kind": "integrated", "args": ["sh"]}}));
                if self.mode == "java-order" {
                    self.event("initialized", json!({}));
                } else {
                    self.reply(req, json!({}));
                }
                self.launched = Some(req.clone());
                self.go();
            }
            "setBreakpoints" => {
                let path = a["source"]["path"].as_str().unwrap_or("").to_string();
                let lines = std::fs::read_to_string(&path).map_or(0, |t| t.lines().count() as u64);
                let bps = a["breakpoints"].as_array().cloned().unwrap_or_default();
                let mut set = vec![];
                for bp in &bps {
                    let line = bp["line"].as_u64().unwrap_or(1);
                    set.push(if line > lines { json!({"verified": false, "line": line, "message": "no code at this line"}) } else { json!({"verified": true, "line": line}) });
                    match bp["logMessage"].as_str() {
                        _ if line > lines => {}
                        Some(text) => self.logs.push(text.into()),
                        None if self.stop_at.is_none() => self.stop_at = Some((path.clone(), line)),
                        None => {}
                    }
                }
                self.reply(req, json!({"breakpoints": set}));
            }
            "setExceptionBreakpoints" if self.script["reject_conditions"] == true && a["filterOptions"].is_array() => self.fail(req, "bad condition"),
            "setExceptionBreakpoints" => {
                self.exceptions = a["filters"].as_array().is_some_and(|f| !f.is_empty()) || a["filterOptions"].is_array();
                self.reply(req, json!({}));
            }
            "configurationDone" => {
                self.reply(req, json!({}));
                self.configured = true;
                self.go();
            }
            "threads" => self.reply(req, json!({"threads": [{"id": self.thread(), "name": self.long("main")}]})),
            "stackTrace" if a["threadId"] != self.thread() => self.fail(req, &format!("invalid thread {}", a["threadId"])),
            "stackTrace" => {
                let frames = self.frames();
                let levels = a["levels"].as_u64().map_or(frames.len(), |l| l as usize);
                self.reply(req, json!({"stackFrames": frames[..levels.min(frames.len())], "totalFrames": frames.len()}));
            }
            "scopes" if self.mode == "eof-on-scopes" => return false,
            "scopes" => {
                let locals = 1000 + a["frameId"].as_u64().unwrap_or(0);
                self.reply(req, json!({"scopes": [{"name": "Locals", "variablesReference": locals}, {"name": "Registers", "variablesReference": 9, "expensive": true}]}));
            }
            "variables" => {
                let vars = self.variables(a["variablesReference"].as_u64().unwrap_or(0));
                self.reply(req, json!({"variables": vars}));
            }
            "exceptionInfo" if self.script["fail_exception_info"] == true => self.fail(req, "no exception info"),
            "exceptionInfo" => {
                let mode = self.script["break_mode"].as_str().unwrap_or("always").to_string();
                self.reply(req, json!({"exceptionId": "AssertionError", "description": "expected 2, got 3", "breakMode": mode}))
            }
            "evaluate" => {
                self.reply(req, json!({"result": format!("{} = 42 in frame {}", a["expression"].as_str().unwrap_or(""), a["frameId"]), "type": "Int", "variablesReference": 0}))
            }
            "continue" if self.mode == "eof-on-continue" => return false,
            "continue" => {
                self.reply(req, json!({"allThreadsContinued": true}));
                self.event("continued", json!({"threadId": 1}));
                if !self.stops.is_empty() {
                    self.stops.remove(0);
                }
                self.next_stop();
            }
            "next" | "stepIn" | "stepOut" => {
                self.reply(req, json!({}));
                self.line = self.frames().iter().find(|f| f["name"] == "orders::save").and_then(|f| f["line"].as_u64()).unwrap_or(1) + 1;
                self.stopped("step");
            }
            "disconnect" => {
                self.reply(req, json!({}));
                return false;
            }
            _ if req["type"] == "response" => {}
            other => {
                let other = other.to_string();
                self.fail(req, &format!("unknown command {other}"));
            }
        }
        true
    }

    fn fail(&mut self, req: &Value, message: &str) {
        self.send(json!({"type": "response", "request_seq": req["seq"], "command": req["command"], "success": false, "message": message}));
    }
}

/// Serve one connection until `disconnect` or its end.
pub fn session(mut input: impl BufRead, out: impl Write, mode: &str) {
    let log = env::var_os("ITSE_FIXTURE_LOG").and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    let (root, script) = (env::current_dir().unwrap_or_default(), env::var("ITSE_FIXTURE_SCRIPT").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default());
    let mut a = Adapter { out, seq: 0, mode: mode.into(), root, stop_at: None, logs: vec![], exceptions: false, stops: vec![], line: 0, launched: None, configured: false, script };
    while let Some(body) = frame(&mut input) {
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        if let Some(mut log) = log.as_ref() {
            let _ = writeln!(log, "{msg}");
        }
        if !a.handle(&msg) {
            return;
        }
    }
}

pub fn serve(args: &[String], mode: &str) -> ExitCode {
    eprintln!("itse-fixture dap: started");
    match args {
        [flag, port] if flag == "--tcp" => {
            let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{port}")) else { return ExitCode::from(2) };
            if let Ok((conn, _)) = listener.accept() {
                session(BufReader::new(conn.try_clone().expect("a clone of the connection")), conn, mode);
            }
        }
        _ => session(io::stdin().lock(), io::stdout(), mode),
    }
    ExitCode::SUCCESS
}
