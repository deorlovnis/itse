//! The client of hosted MCP servers: an extension's MCP server, started on demand through the
//! session host's shared start (stdio, one JSON value per line), stopped when idle, and started
//! again by the next call after a crash, as a language server is.
//!
//! A server's tool list is cached in the workspace's state, keyed by the sha256 of its binary (the
//! package's pin when it runs from the store) and its launch, so listing itse's own tools never
//! cold-starts it; each start lists again, as an interpreter's script may have changed. A
//! server the developer layer exposes (`"expose": true`) has its tools appended to itse's list as
//! `<server>__<tool>`, each described by its first sentence and dropped once the list is full, and
//! a call to one passes straight through. What a server sends is data: its requests for sampling,
//! elicitation or roots are refused, and only `ping` is answered.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, fs::File, io::Write, mem, thread};

use anyhow::{Result, anyhow, ensure};
use serde_json::{Map, Value, json};

use crate::host::{Asked, Host, Launch, Live, Phase, Protocol, Server, Slot};
use crate::resolve::{Resolved, resolve_for};
use crate::results::{Outcome, Verdict, text};
use crate::rpc::{Cancel, Dialect, Framing, Handler, Peer, RpcError};
use crate::usage::clip;
use crate::verbs::{self, Answer, Caller};
use crate::{sys, sys::lock};

/// The protocol versions a hosted server may answer `initialize` with.
pub const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const INIT: Duration = Duration::from_secs(30);
const CALL: Duration = Duration::from_secs(120);
/// How long itse's own `tools/list` waits for an exposed server that has no list cached yet.
const FIRST: Duration = Duration::from_secs(10);
const MAX_TOOLS: usize = 200;
/// A server's whole list, as cached, stops growing past this: a tool larger than the list cap is
/// never exposed anyway.
const MAX_LIST: usize = 1 << 20;
/// MCP clients refuse longer tool names (Claude's must match `^[a-zA-Z0-9_-]{1,64}$`).
const MAX_NAME: usize = 64;
const MAX_ABOUT: usize = 160;
/// A tool's annotations that a listing keeps: a client runs a read-only tool alongside others,
/// and asks before a destructive one.
const HINTS: [&str; 4] = ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"];
/// An exposed server's share of the list unless the developer layer sets `max_list_bytes`.
const SERVER_BYTES: usize = 1200;

/// The reader's side: it answers the server's `ping`, refuses its other requests, and marks the
/// cached list stale when the server says its tools changed. It touches only the slot's `Live`.
struct Reader {
    live: Arc<Mutex<Live>>,
    epoch: u64,
    log: Mutex<File>,
}

impl Handler for Reader {
    fn notify(&self, method: &str, _params: Value) {
        let mut live = lock(&self.live);
        if live.epoch == self.epoch && method == "notifications/tools/list_changed" {
            live.tools_changed = true;
        }
    }

    fn request(&self, method: &str, _params: Value) -> Result<Value, (i64, String)> {
        match method {
            "ping" => Ok(json!({})),
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    fn skipped(&self, why: &str) {
        let _ = writeln!(lock(&self.log), "{} skipped: {why}", sys::utc(SystemTime::now()));
    }
}

/// Start the server over the host's shared start, with MCP's lines and `initialize`. Its list is
/// asked for again once it is up.
fn start(host: &Host, slot: &Slot, l: &Launch, env: &BTreeMap<String, String>) -> Result<Server> {
    let reader = |epoch, file| Arc::new(Reader { live: slot.live.clone(), epoch, log: Mutex::new(file) }) as Arc<dyn Handler>;
    host.start_server(slot, "mcp", l, env, Framing::Lines, Dialect::Mcp, reader, |s| {
        initialize(&s.peer)?;
        lock(&slot.live).tools_changed = true;
        Ok(())
    })
}

/// `initialize` (30 s); a server that answers with a version itse doesn't speak is refused, else
/// it gets `notifications/initialized`.
fn initialize(peer: &Peer) -> Result<()> {
    let params = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "itse", "version": env!("CARGO_PKG_VERSION")}});
    let reply = peer.request("initialize", params, INIT, &Cancel::default()).map_err(|e| anyhow!("before initialize answered (initialize: {e})"))?;
    let version = reply["protocolVersion"].as_str().unwrap_or("");
    ensure!(VERSIONS.contains(&version), "at initialize (mcp_version_unsupported: it answered '{}'; itse speaks {})", clip(version), VERSIONS.join(", "));
    let _ = peer.notify("notifications/initialized", json!({}));
    Ok(())
}

/// The workspace resolved for a caller, again for every call, so a manifest edit applies at the
/// next start. A hosted server left running that no session may start any more (a revoked trust,
/// an unused extension) is stopped; one only this caller can't start (a `path` install off its
/// PATH) runs on for the session that started it.
pub fn resolved(host: &Host, caller: &BTreeMap<String, String>) -> Result<Resolved> {
    let r = resolve_for(&host.ws, &host.dirs, caller, SystemTime::now(), false)?;
    let uses = r.manifest.uses();
    let declared = |id: &str| uses.iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| &e.0.servers).any(|s| s.protocol == Protocol::Mcp && s.id == id);
    let in_use = |id: &str| declared(id) && !r.held.iter().any(|h| h == id);
    let left: Vec<String> = lock(&host.slots).keys().filter(|h| h.strip_prefix("mcp:").is_some_and(|id| !in_use(id))).cloned().collect();
    left.iter().for_each(|h| _ = host.stop_server(h, Duration::from_secs(5)));
    Ok(r)
}

/// `mcp:<id>` up for a call, started if it must be.
fn up(host: &Host, r: &Resolved, l: &Launch, cancel: &Cancel) -> Result<Asked> {
    let asked = host.ask("mcp", l, true, cancel, |slot| start(host, slot, l, &host.server_env(r)?))?;
    asked.ok_or_else(|| anyhow!("mcp:{} did not start", l.id))
}

/// What a server's cached list is keyed by: the pin of the store package its command runs from,
/// else the sha256 of the binary itself (streamed, and hashed again only when its size or mtime
/// changes), with the launch's hash, so another argv or env lists again.
fn key(host: &Host, l: &Launch) -> String {
    static HASHED: Mutex<BTreeMap<PathBuf, (u64, i64, String)>> = Mutex::new(BTreeMap::new());
    let bin = PathBuf::from(l.argv.first().map_or("", String::as_str));
    let pin = bin.strip_prefix(host.dirs.data.join("store")).ok().and_then(|p| p.components().next()?.as_os_str().to_str()?.strip_prefix("sha256-").map(String::from));
    let sha = match (pin, fs::metadata(&bin)) {
        (Some(pin), _) => pin,
        (None, Err(_)) => String::new(),
        (None, Ok(meta)) => {
            let mut hashed = lock(&HASHED);
            match hashed.get(&bin) {
                Some((size, mtime, sha)) if (*size, *mtime) == (meta.size(), meta.mtime()) => sha.clone(),
                _ => {
                    let sha = crate::store::file_sha256(&bin).unwrap_or_default();
                    hashed.insert(bin, (meta.size(), meta.mtime(), sha.clone()));
                    sha
                }
            }
        }
    };
    format!("{sha}-{}", crate::host::launch_hash(l))
}

/// The server said its list changed since it was cached, or was started since: the next listing
/// asks again. Only while it is up, so a listing never starts a server that was stopped.
fn changed(host: &Host, l: &Launch) -> bool {
    let slot = host.slot(&format!("mcp:{}", l.id));
    let up = matches!(slot.phase.try_lock().as_deref(), Ok(Phase::Up(s)) if s.peer.closed().is_none());
    up && lock(&slot.live).tools_changed
}

fn cache_path(host: &Host, id: &str) -> PathBuf {
    host.ws.state_dir(&host.dirs).join("mcp").join(format!("{id}.json"))
}

/// The tools `l` listed last, unless its binary changed since.
fn cached(host: &Host, l: &Launch) -> Option<Vec<Value>> {
    let mut cache: Value = serde_json::from_slice(&fs::read(cache_path(host, &l.id)).ok()?).ok()?;
    (cache["key"] == key(host, l)).then(|| cache["tools"].as_array_mut().map(mem::take)).flatten()
}

/// Ask the server for its tools, following `nextCursor` up to 200 tools, 1 MB and 30 s in all,
/// and cache what a listing uses of each (see `listed`); with the notes of
/// its start, if this call started it. A page without tools, or a cursor seen before, ends the
/// list, so a server that pages forever can't hold a listing.
fn list(host: &Host, r: &Resolved, l: &Launch, cancel: &Cancel) -> Result<(Vec<Value>, Vec<String>)> {
    let mut up = up(host, r, l, cancel)?;
    lock(&up.slot.live).tools_changed = false;
    let (mut tools, mut bytes, mut seen, mut cursor, end) = (Vec::new(), 0, BTreeSet::new(), Value::Null, Instant::now() + INIT);
    'pages: while tools.len() < MAX_TOOLS {
        ensure!(!cancel.is_cancelled(), "cancelled");
        let params = if cursor.is_string() { json!({"cursor": cursor}) } else { json!({}) };
        let page = up.server.peer.request("tools/list", params, end.saturating_duration_since(Instant::now()), cancel);
        let mut page = page.map_err(|e| match e {
            RpcError::Timeout => anyhow!("mcp:{}: tools/list did not end in {} s", l.id, INIT.as_secs()),
            e => anyhow!("mcp:{}: tools/list: {e}", l.id),
        })?;
        let served = page["tools"].as_array_mut().map(mem::take).unwrap_or_default();
        if served.is_empty() {
            break;
        }
        for t in served.into_iter().take(MAX_TOOLS - tools.len()) {
            let tool = listed(t["name"].clone(), &t, t["inputSchema"].clone());
            bytes += tool.to_string().len();
            if bytes > MAX_LIST {
                up.notes.push(format!("mcp:{}: tools past {} MB in all not listed", l.id, MAX_LIST >> 20));
                break 'pages;
            }
            tools.push(tool);
        }
        cursor = page["nextCursor"].take();
        if !cursor.as_str().is_some_and(|c| seen.insert(c.to_string())) {
            break;
        }
    }
    let path = cache_path(host, &l.id);
    sys::private_dir(path.parent().unwrap_or(&path))?;
    sys::write_atomic(&path, json!({"key": key(host, l), "tools": tools}).to_string().as_bytes())?;
    lock(&up.slot.live).used = Instant::now();
    Ok((tools, up.notes))
}

/// A description's first sentence, at most 160 chars: an agent reads the rest with the schema.
/// The first paragraph's lines are joined first, as a docstring wraps its sentences.
fn about(description: &str) -> String {
    let words: Vec<&str> = description.lines().skip_while(|l| l.trim().is_empty()).take_while(|l| !l.trim().is_empty()).flat_map(str::split_whitespace).collect();
    let para = words.join(" ");
    let end = para.match_indices(['.', '?', '!']).map(|(i, _)| i).find(|&i| para[i + 1..].starts_with(' '));
    end.map_or(para.as_str(), |i| &para[..=i]).chars().take(MAX_ABOUT).collect()
}

/// A server's tool as a listing carries it, named `name`: its first sentence, its schema and its
/// boolean hints. Its title and output schema stay out, to save the list's bytes.
fn listed(name: Value, t: &Value, schema: Value) -> Value {
    let mut tool = json!({"name": name, "description": about(t["description"].as_str().unwrap_or("")), "inputSchema": schema});
    let hints: Map<String, Value> = HINTS.iter().filter_map(|&k| Some((k.to_string(), Value::Bool(t["annotations"][k].as_bool()?)))).collect();
    if !hints.is_empty() {
        tool["annotations"] = Value::Object(hints);
    }
    tool
}

/// A server's tool as itse names it: `<server>__<tool>`, with `-` as `_`.
fn exposed_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}").replace('-', "_")
}

/// Itse's tool list: its own tools, then each exposed server's that fit, from their cached lists,
/// with what `status` says about the rest.
#[derive(Default)]
pub struct Listing {
    pub tools: Vec<Value>,
    /// The `tools/list` result's bytes plus `instructions`, as the cap counts them.
    pub bytes: usize,
    pub notes: Vec<String>,
    /// Exposed name → the server's id and its own name for the tool.
    routes: BTreeMap<String, (String, String)>,
    /// Exposed servers with no list cached yet, or whose list changed.
    unlisted: Vec<String>,
    r: Option<Resolved>,
}

impl Listing {
    fn new(core: Vec<Value>) -> Self {
        let bytes = json!({"tools": core}).to_string().len() + text("instructions", &[]).len();
        Self { tools: core, bytes, ..Self::default() }
    }

    /// Append `server`'s tools in its own order, those its allowlist names, until one doesn't fit
    /// its share (`max_list_bytes`) or the list cap: that one and the rest are dropped, and named
    /// in a note. Names over 64 chars or with characters other than letters, digits and `_` are
    /// skipped, as is a name that another tool's took (`a-b` and `a_b` are both `a_b`).
    fn expose(&mut self, server: &str, allow: Option<&[String]>, share: usize, tools: &[Value]) {
        let allowed: Vec<&Value> = tools.iter().filter(|t| allow.is_none_or(|names| names.iter().any(|n| t["name"] == *n))).collect();
        let (mut used, mut long, mut odd, mut clash, mut dropped, mut full) = (0, 0, 0, 0, 0, "");
        for t in &allowed {
            let own = t["name"].as_str().unwrap_or("");
            let name = exposed_name(server, own);
            if name.len() > MAX_NAME {
                long += 1;
                continue;
            }
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                odd += 1;
                continue;
            }
            if self.routes.contains_key(&name) {
                clash += 1;
                continue;
            }
            let schema = if t["inputSchema"].is_object() { t["inputSchema"].clone() } else { json!({"type": "object"}) };
            let tool = listed(json!(name), t, schema);
            // A comma joins it to the list.
            let bytes = tool.to_string().len() + 1;
            if dropped == 0 && used + bytes > share {
                full = "max_list_bytes";
            } else if dropped == 0 && self.bytes + bytes > crate::mcp::LIST_CAP {
                full = "list cap";
            }
            if !full.is_empty() {
                dropped += 1;
                continue;
            }
            (used, self.bytes) = (used + bytes, self.bytes + bytes);
            self.routes.insert(name, (server.into(), own.into()));
            self.tools.push(tool);
        }
        if long > 0 {
            self.notes.push(format!("{server}: {} skipped, named over {MAX_NAME} chars as {server}__<tool>", crate::parse::plural(long, "tool")));
        }
        if odd > 0 {
            self.notes.push(format!("{server}: {} skipped, named with characters MCP clients refuse", crate::parse::plural(odd, "tool")));
        }
        if clash > 0 {
            self.notes.push(format!("{server}: {} skipped, named as another tool is once '-' is '_'", crate::parse::plural(clash, "tool")));
        }
        if dropped > 0 {
            let limit = if full == "list cap" { "list cap 3,600 B".to_string() } else { format!("max_list_bytes {share} B") };
            self.notes.push(format!("{server}: {dropped} of {} tools dropped ({limit}); set tools: [...]", allowed.len() - long - odd - clash));
        }
    }
}

/// Itse's tool list for a caller, without starting anything: the core tools and the exposed
/// servers' cached tools. A server held for trust is named, never started.
pub fn listing(host: &Host, caller: &BTreeMap<String, String>, debug: bool) -> Listing {
    let mut out = Listing::new(crate::mcp::tools(debug));
    let r = match resolved(host, caller) {
        Ok(r) => r,
        Err(e) => {
            out.notes.push(format!("mcp: {e:#}"));
            return out;
        }
    };
    let mcp = |id: &String| r.manifest.exts.values().flat_map(|e| &e.0.servers).any(|s| s.id == *id && s.protocol == Protocol::Mcp);
    out.notes.extend(r.held.iter().filter(|id| mcp(id)).map(|id| format!("{id}: not started: runs project code; next (human): itse trust")));
    for l in r.launches.iter().filter(|l| l.protocol == Protocol::Mcp && l.expose) {
        // A changed list is asked for again, and its tools stay callable meanwhile.
        let tools = cached(host, l);
        if tools.is_none() || changed(host, l) {
            out.unlisted.push(l.id.clone());
        }
        match tools {
            Some(tools) => out.expose(&l.id, l.tools.as_deref(), l.max_list_bytes.unwrap_or(SERVER_BYTES), &tools),
            None => out.notes.push(format!("{}: tools not listed yet (first start)", l.id)),
        }
    }
    out.r = Some(r);
    out
}

/// Itse's own `tools/list`. An exposed server with no list yet is started and listed, waited for
/// up to 10 s; a list that comes later is cached, and the receiver hears whether one was, so the
/// session can tell its harness that the list changed.
pub fn tools_list(host: &Arc<Host>, caller: &BTreeMap<String, String>, debug: bool) -> (Listing, Option<mpsc::Receiver<bool>>) {
    let mut first = listing(host, caller, debug);
    let Some(r) = first.r.take().filter(|_| !first.unlisted.is_empty()) else { return (first, None) };
    let (tx, rx) = mpsc::channel();
    let (host2, unlisted) = (host.clone(), mem::take(&mut first.unlisted));
    thread::spawn(move || {
        let mut filled = false;
        for l in r.launches.iter().filter(|l| l.protocol == Protocol::Mcp && unlisted.contains(&l.id)) {
            match list(&host2, &r, l, &Cancel::default()) {
                Ok(_) => filled = true,
                Err(e) => eprintln!("itse daemon: mcp:{}: {e:#}", l.id),
            }
        }
        let _ = tx.send(filled);
    });
    match rx.recv_timeout(FIRST) {
        Ok(_) => (listing(host, caller, debug), None),
        Err(_) => (first, Some(rx)),
    }
}

/// A call to an exposed tool, passed through to its server (120 s, or its `timeout_s`); None if
/// `name` is no tool in the list as it stands. A failure is a tool result with `isError`; a
/// server that ended under the call is `server_crash`, and the next call starts it again and
/// says so in a last text block. The usage log gets the class `ext_tool`, never the tool's name.
/// A name the cached list doesn't route, of an exposed server whose list is missing or stale (its
/// binary or launch changed), is routed after that server is listed again.
pub fn call(host: &Arc<Host>, caller: &Caller, name: &str, arguments: Value, cancel: &Cancel) -> Option<Value> {
    let (started, debug) = (Instant::now(), host.config().mcp.debug);
    let mut listing = listing(host, &caller.env, debug);
    if !listing.routes.contains_key(name) {
        let r = listing.r.as_ref()?;
        let l = r.launches.iter().find(|l| listing.unlisted.contains(&l.id) && name.starts_with(&exposed_name(&l.id, "")))?;
        let _ = list(host, r, l, cancel).inspect_err(|e| eprintln!("itse daemon: mcp:{}: {e:#}", l.id));
        listing = self::listing(host, &caller.env, debug);
    }
    let ((id, own), r) = (listing.routes.get(name)?.clone(), listing.r?);
    let l = r.launches.iter().find(|l| l.protocol == Protocol::Mcp && l.id == id)?;
    let timeout = l.timeout_s.map_or(CALL, Duration::from_secs);
    // A call without arguments has none, not null: a server's schema check refuses null.
    let arguments = if arguments.is_object() { arguments } else { json!({}) };
    let called = up(host, &r, l, cancel).map_err(|e| ("server_start", format!("mcp:{id}: {e:#}"))).and_then(|up| {
        let reply = up.server.peer.request("tools/call", json!({"name": own, "arguments": arguments}), timeout, cancel);
        lock(&up.slot.live).used = Instant::now();
        let reply = reply.map_err(|e| match e {
            RpcError::Closed(why) => ("server_crash", format!("server_crash: mcp:{id} ended during the call ({why}); the next call starts it again")),
            RpcError::Timeout => ("timeout", format!("mcp:{id}: timeout: no answer in {} s", timeout.as_secs())),
            RpcError::Cancelled => ("cancel", text("cancelled", &[])),
            e => ("io", format!("mcp:{id}: {e}")),
        })?;
        ensure_object(reply, &id).map(|reply| (reply, up.notes))
    });
    let (reply, verdict, err) = match called {
        Ok((mut reply, notes)) => {
            if let Some(content) = reply.get_mut("content").and_then(Value::as_array_mut).filter(|_| !notes.is_empty()) {
                content.push(json!({"type": "text", "text": notes.join("\n")}));
            }
            let verdict = if reply["isError"] == true { Verdict::Fail } else { Verdict::Ok };
            (reply, verdict, None)
        }
        Err((class, e)) => (json!({"content": [{"type": "text", "text": format!("ERROR {name} — {e}\n")}], "isError": true}), Verdict::Error, Some(class)),
    };
    // An added extension's ids are the developer's own words: only a built-in's id is logged.
    let ext = r.manifest.exts.iter().find(|(_, (e, origin))| *origin == "built-in" && e.servers.iter().any(|s| s.id == id)).map(|(ext, _)| ext.clone());
    let answer = Answer { verdict, text: reply.to_string(), err, ..Answer::default() };
    verbs::record(&host.usage, caller, "ext_tool", ext, started.elapsed().as_millis() as u64, &answer);
    Some(reply)
}

/// A tool result is an object; anything else from the server is an io error, not passed on.
fn ensure_object(reply: Value, id: &str) -> Result<Value, (&'static str, String)> {
    match reply {
        Value::Object(_) => Ok(reply),
        other => Err(("io", format!("mcp:{id}: tools/call answered {}, not a result object", clip(&other.to_string())))),
    }
}

/// `itse ext tools <server>`: a hosted server's tools, each with its first sentence and the name
/// it has in itse's list, from its cached list or, without one, from the server, started if it
/// must be. A server that runs project code isn't started before trust.
pub fn ext_tools(host: &Host, caller: &Caller, id: &str, cancel: &Cancel) -> Outcome {
    let scope = format!("tools {id}");
    let r = match resolved(host, &caller.env) {
        Ok(r) => r,
        Err(e) => return Outcome::error("ext", "bad_args", format!("{e:#}")),
    };
    let Some(l) = r.launches.iter().find(|l| l.protocol == Protocol::Mcp && l.id == id) else {
        if r.held.iter().any(|h| h == id) {
            let states = r.states.iter().filter(|s| s.state == "untrusted").cloned().collect();
            return Outcome { states, err: Some("untrusted"), ..Outcome::new("ext", Verdict::Untrusted, scope, format!("{id} runs project code")) };
        }
        return Outcome::error("ext", "bad_args", format!("no MCP server '{id}' in use; itse status lists the servers"));
    };
    let listed = cached(host, l).filter(|_| !changed(host, l)).map_or_else(|| list(host, &r, l, cancel), |tools| Ok((tools, vec![])));
    let (tools, mut notes) = match listed {
        Ok(listed) => listed,
        Err(e) => return Outcome::error("ext", if format!("{e:#}").contains("start_failed") { "server_start" } else { "io" }, format!("mcp:{id}: {e:#}")),
    };
    let lines = tools.iter().map(|t| {
        let own = t["name"].as_str().unwrap_or("");
        format!("{} — {}", exposed_name(id, own), about(t["description"].as_str().unwrap_or("")))
    });
    notes.extend(lines);
    let shown = if l.expose { "exposed" } else { "not exposed; \"expose\": true in the developer layer lists them over MCP" };
    Outcome { notes, ..Outcome::new("ext", Verdict::Ok, scope, format!("{} tools; {shown}", tools.len())) }
}

#[cfg(test)]
mod tests;
