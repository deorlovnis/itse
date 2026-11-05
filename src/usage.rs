//! The usage log (EMA-212, design §8): one JSON line per call in `usage.jsonl`, on by default, and
//! `itse stats` over it. Nothing is uploaded, ever: `cat` is the export and `rm` the forget.
//!
//! A record holds no values: no arguments, paths, symbols, test names, result text, error
//! messages or env values. Every string is an id, a hash, or a member of a closed set. Writing
//! fails open: a call never fails because its line couldn't be written. Each line opens, locks,
//! writes and closes the file (Halti's audit log), so the daemons of several workspaces share it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::manifest::Config;
use crate::sys::{self, lock};
use crate::verbs::Args;
use crate::workspace::Dirs;

/// Past this, the writer that holds the lock renames the log to `usage.1.jsonl` before appending.
const ROTATE_AT: u64 = 33_554_432;

/// One line of `usage.jsonl`.
#[derive(Debug, Serialize)]
pub struct Record {
    pub v: u32,
    /// UTC with milliseconds.
    pub ts: String,
    /// The first 12 hex of sha256(salt ‖ canonical root).
    pub ws: String,
    /// 16 hex: the hashed session key, or random bytes per connection or call.
    pub sess: String,
    #[serde(flatten)]
    pub ev: Event,
}

#[derive(Debug, Serialize)]
#[serde(tag = "ev", rename_all = "lowercase")]
pub enum Event {
    /// An MCP session began.
    Init {
        client: String,
        client_v: Option<String>,
        proto: String,
        tools: u32,
        list_bytes: u64,
    },
    Call(Call),
}

#[derive(Debug, Serialize)]
pub struct Call {
    /// `cli`, `mcp` or `hook`.
    pub via: &'static str,
    /// Clipped to 64 chars, control characters as `?`.
    pub client: String,
    pub client_v: Option<String>,
    /// From the closed verb table, `<server>__<tool>`, or `unknown`.
    pub verb: String,
    pub ext: Option<String>,
    pub ms: u64,
    /// `ok`, `fail`, `error`, `cancel` or `timeout`.
    pub outcome: &'static str,
    pub err: Option<&'static str>,
    pub bytes: u64,
    pub full: u64,
    pub trunc: bool,
    pub warm: Option<bool>,
    pub settled: Option<bool>,
}

/// Who calls, as ids only (§3.4). `session` keys stored results and baselines: the hashed
/// `ITSE_SESSION`, else `cli` for a shell, else one per MCP connection. `sess` is the log's: the
/// same hash, else random per connection, so calls of a shell aren't joined.
#[derive(Clone, Debug, Default)]
pub struct Ident {
    pub session: String,
    pub sess: String,
    pub client: String,
    pub client_v: Option<String>,
}

/// The writer of one process: the daemon's, or a short-lived one for a local verb.
pub struct Usage {
    state: PathBuf,
    config: PathBuf,
    root: PathBuf,
    /// ITSE_USAGE=off in this process's own env.
    off: bool,
    /// Whether the one `usage log:` line per lifetime is still to be written to stderr (daemon.log).
    warn: AtomicBool,
    salt: OnceLock<Option<[u8; 32]>>,
    /// `"usage": false` in config.json, re-read when its mtime changes.
    config_off: Mutex<(Option<SystemTime>, bool)>,
}

impl Usage {
    pub fn new(dirs: &Dirs, root: &Path, warn: bool) -> Self {
        let off = std::env::var("ITSE_USAGE").is_ok_and(|v| v == "off");
        let config_off = Mutex::new((None, false));
        Self { state: dirs.state.clone(), config: dirs.config.join("config.json"), root: root.into(), off, warn: AtomicBool::new(warn), salt: OnceLock::new(), config_off }
    }

    pub fn path(&self) -> PathBuf {
        self.state.join("usage.jsonl")
    }

    /// The first `n` hex digits of sha256(salt ‖ value): ids that can't be reversed by guessing.
    fn hash(&self, value: &[u8], n: usize) -> Option<String> {
        let salt = self.salt.get_or_init(|| sys::salt(&self.state).ok()).as_ref()?;
        Some(sys::sha256_hex(&[&salt[..], value].concat())[..n].to_string())
    }

    pub fn ident(&self, mode: &str, env: &BTreeMap<String, String>) -> Ident {
        let random = sys::hex(&sys::random_bytes::<8>().unwrap_or_default());
        let key = env.get("ITSE_SESSION").filter(|s| !s.is_empty()).map(|s| self.hash(s.as_bytes(), 16).unwrap_or_else(|| random.clone()));
        let session = key.clone().unwrap_or_else(|| if mode == "mcp" { random.clone() } else { "cli".into() });
        let (client, client_v) = env.get("ITSE_CLIENT").map_or(("cli", None), |c| c.split_once('/').map_or((c.as_str(), None), |(n, v)| (n, Some(clip(v)))));
        Ident { session, sess: key.unwrap_or(random), client: clip(client), client_v }
    }

    /// Off by ITSE_USAGE=off here or in the caller's env, or by `"usage": false` in config.json.
    pub fn on(&self, env: &BTreeMap<String, String>) -> bool {
        let mtime = fs::metadata(&self.config).and_then(|m| m.modified()).ok();
        let mut cfg = lock(&self.config_off);
        if cfg.0 != mtime {
            let config: Option<Config> = fs::read(&self.config).ok().and_then(|b| serde_json::from_slice(&b).ok());
            *cfg = (mtime, config.and_then(|c| c.usage) == Some(false));
        }
        !self.off && !cfg.1 && env.get("ITSE_USAGE").is_none_or(|v| v != "off")
    }

    pub fn record(&self, env: &BTreeMap<String, String>, sess: &str, ev: Event) {
        if !self.on(env) {
            return;
        }
        let ws = self.hash(self.root.as_os_str().as_encoded_bytes(), 12).ok_or_else(|| anyhow!("no salt in {}", self.state.display()));
        let line = ws.and_then(|ws| Ok(serde_json::to_string(&Record { v: 1, ts: sys::utc(SystemTime::now()), ws, sess: sess.into(), ev })? + "\n"));
        let written = line.and_then(|line| self.append(&line));
        if let Err(e) = written
            && self.warn.swap(false, Ordering::SeqCst)
        {
            eprintln!("itse daemon: usage log: {e:#}");
        }
    }

    /// Append one line under the file's lock. A writer that finds the log past `ROTATE_AT` renames
    /// it first; one whose open file was renamed meanwhile opens the path again.
    fn append(&self, line: &str) -> Result<()> {
        let path = self.path();
        sys::private_dir(&self.state)?;
        let open = || OpenOptions::new().append(true).create(true).mode(0o600).open(&path);
        for _ in 0..3 {
            let file = open()?;
            file.lock()?;
            let meta = file.metadata()?;
            if fs::metadata(&path).map(|m| m.ino()).ok() != Some(meta.ino()) {
                continue;
            }
            let target: File = if meta.len() > ROTATE_AT {
                fs::rename(&path, self.state.join("usage.1.jsonl"))?;
                let fresh = open()?;
                fresh.lock()?;
                fresh
            } else {
                file
            };
            return Ok((&target).write_all(line.as_bytes())?);
        }
        bail!("{} kept changing while locked", path.display())
    }
}

/// At most 64 chars, control characters as `?`.
pub fn clip(s: &str) -> String {
    s.chars().take(64).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Counts of one group of calls.
#[derive(Default)]
struct Group {
    outcomes: BTreeMap<String, u64>,
    ms: Vec<u64>,
    bytes: u64,
    kept: u64,
    trunc: u64,
    warm: (u64, u64),
}

/// `itse stats [--since 7d|24h|YYYY-MM-DD] [--by verb|ext|client|ws|day] [--json]`, read from
/// `usage.1.jsonl` then `usage.jsonl`. It only reads, and is not logged. `offered` are the verbs
/// the MCP tools carry now, for "offered, never called".
pub fn stats(dirs: &Dirs, args: &Args, offered: &[&str]) -> Result<String> {
    let flag = |k: &str| args.flags.get(k).and_then(Value::as_str);
    let (span, by) = (flag("since").unwrap_or("7d"), flag("by").unwrap_or("verb"));
    // A span longer than the epoch counts from it.
    let back = |secs: Option<u64>| secs.and_then(|s| SystemTime::now().checked_sub(Duration::from_secs(s))).unwrap_or(UNIX_EPOCH);
    let ago = |unit: u64| span[..span.len() - 1].parse::<u64>().ok().map(|n| sys::utc(back(n.checked_mul(unit))));
    let since = match span.as_bytes().last() {
        Some(b'd') => ago(86_400),
        Some(b'h') => ago(3600),
        _ => regex_lite::Regex::new(r"^\d{4}-\d\d-\d\d$").ok().filter(|re| re.is_match(span)).map(|_| span.to_string()),
    };
    let Some(since) = since else { bail!("--since takes 7d, 24h or YYYY-MM-DD, not '{span}'") };
    if !["verb", "ext", "client", "ws", "day"].contains(&by) {
        bail!("--by takes verb, ext, client, ws or day, not '{by}'");
    }
    let (mut groups, mut inits, mut callers, mut wss, mut called, mut errors) =
        (BTreeMap::<String, Group>::new(), BTreeMap::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeMap::new());
    let mut size = 0;
    for name in ["usage.1.jsonl", "usage.jsonl"] {
        let Ok(file) = File::open(dirs.state.join(name)) else { continue };
        size += file.metadata().map_or(0, |m| m.len());
        for r in BufReader::new(file).lines().map_while(io::Result::ok).filter_map(|l| serde_json::from_str::<Value>(&l).ok()) {
            let (s, sess) = (|k: &str| r[k].as_str().unwrap_or("-").to_string(), r["sess"].as_str().unwrap_or("").to_string());
            if s("ts").as_str() < since.as_str() {
                continue;
            }
            wss.insert(s("ws"));
            let client = format!("{} {}", s("client"), r["client_v"].as_str().unwrap_or("")).trim().to_string();
            if r["ev"] == "init" {
                inits.insert(sess, client);
                continue;
            }
            callers.insert(sess);
            called.insert(s("verb"));
            let key = match by {
                "verb" if r["ext"].is_string() => format!("{} ({})", s("verb"), s("ext")),
                "day" => s("ts")[..10.min(s("ts").len())].to_string(),
                "client" => client,
                k => s(k),
            };
            let g = groups.entry(key).or_default();
            *g.outcomes.entry(s("outcome")).or_default() += 1;
            g.ms.push(r["ms"].as_u64().unwrap_or(0));
            let (bytes, full) = (r["bytes"].as_u64().unwrap_or(0), r["full"].as_u64().unwrap_or(0));
            (g.bytes, g.kept, g.trunc) = (g.bytes + bytes, g.kept + full.saturating_sub(bytes), g.trunc + u64::from(r["trunc"] == true));
            g.warm = (g.warm.0 + u64::from(r["warm"] == true), g.warm.1 + u64::from(r["warm"].is_boolean()));
            if r["err"].is_string() {
                *errors.entry((s("err"), r["ext"].as_str().map(String::from))).or_insert(0) += 1;
            }
        }
    }
    let idle: Vec<&String> = inits.iter().filter(|(s, _)| !callers.contains(*s)).map(|(_, c)| c).collect();
    let sessions = callers.union(&inits.keys().cloned().collect()).count();
    let never: Vec<&str> = offered.iter().copied().filter(|v| !called.contains(*v)).collect();
    let calls: u64 = groups.values().map(|g| g.ms.len() as u64).sum();
    let until = sys::utc(SystemTime::now());
    groups.values_mut().for_each(|g| g.ms.sort_unstable());
    let pct = |ms: &[u64], p: usize| ms.get(((ms.len() * p).div_ceil(100)).max(1) - 1).copied().unwrap_or(0);
    let rate = |n: u64, of: u64| if n == 0 || of == 0 { "-".to_string() } else { format!("{}%", (n * 100 + of / 2) / of) };
    if args.flags.contains_key("json") {
        let groups: Vec<Value> = groups
            .into_iter()
            .map(|(key, g)| {
                let (n, o) = (g.ms.len() as u64, |k: &str| g.outcomes.get(k).copied().unwrap_or(0));
                json!({"key": key, "calls": n, "ok": o("ok"), "fail": o("fail"), "error": o("error"), "cancel": o("cancel"), "timeout": o("timeout"), "p50_ms": pct(&g.ms, 50),
                "p95_ms": pct(&g.ms, 95), "bytes": g.bytes, "tok_est": g.bytes / 4, "kept_bytes": g.kept, "trunc_rate": g.trunc as f64 / n as f64,
                "warm_rate": (g.warm.1 > 0).then(|| g.warm.0 as f64 / g.warm.1 as f64)})
            })
            .collect();
        return Ok(
            json!({"since": since, "until": until, "calls": calls, "sessions": sessions, "groups": groups, "idle_sessions": idle.len(), "never_called": never}).to_string() + "\n"
        );
    }
    let mut out = format!("usage {}..{} ({span}): {} calls, {sessions} sessions, {} workspaces; log {}\n", &since[..10], &until[..10], num(calls), wss.len(), mb(size));
    let w = groups.keys().map(|k| k.chars().count()).max().unwrap_or(0).max(15);
    let row = |c: &[&str]| format!("{:<w$} {:>5}  {:<4} {:<4} {:<4} {:>6} {:>6} {:>10} {:>9} {:>6} {:>5}\n", c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10]);
    out += &row(&[by, "calls", "ok", "fail", "err", "p50ms", "p95ms", "~tok/call", "kept out", "trunc", "warm"]);
    for (key, g) in &groups {
        let (n, o) = (g.ms.len() as u64, |k: &str| g.outcomes.get(k).copied().unwrap_or(0));
        let (ok, fail, err) = (rate(o("ok"), n), rate(o("fail"), n), rate(n - o("ok") - o("fail"), n));
        let (p50, p95, kept) = (num(pct(&g.ms, 50)), num(pct(&g.ms, 95)), if g.kept == 0 { "-".into() } else { mb(g.kept) });
        out += &row(&[key, &num(n), &ok, &fail, &err, &p50, &p95, &num(g.bytes / 4 / n), &kept, &rate(g.trunc, n), &rate(g.warm.0, g.warm.1)]);
    }
    if !idle.is_empty() {
        let clients: BTreeSet<&String> = idle.iter().copied().collect();
        out += &format!("connected, never called: {} of {sessions} sessions ({})\n", idle.len(), clients.into_iter().cloned().collect::<Vec<_>>().join(", "));
    }
    if !never.is_empty() {
        out += &format!("offered, never called: {}\n", never.join(", "));
    }
    if !errors.is_empty() {
        let list: Vec<String> = errors.iter().map(|((e, ext), n)| format!("{e} {n}{}", ext.as_ref().map_or(String::new(), |x| format!(" ({x})")))).collect();
        out += &format!("errors: {}\n", list.join(", "));
    }
    Ok(out)
}

/// `1,284`.
fn num(n: u64) -> String {
    let digits = n.to_string();
    digits.as_bytes().rchunks(3).rev().map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(",")
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests;
