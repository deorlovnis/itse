//! The LSP client: the `initialize` handshake, the answers to a server's own requests, what the
//! reader thread keeps (diagnostics, progress, server status, messages), the sync from disk before
//! every query, and `settle`, which says whether the server has caught up.
//!
//! Agents edit files outside Itse, so every query first tells the server what changed on disk: the
//! documents opened for queries are compared by mtime, size and inode, and the others come from a
//! read-only `git status` on every query (a few milliseconds), and `git diff` between the old and
//! the new HEAD when it moved; those are opened from disk and saved. Itse writes nothing into the
//! repo: git runs with its optional locks off, so it never takes `.git/index.lock` or rewrites the
//! index, and with nothing of the repo's config or hooks that names a command.

use std::os::unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt};
use std::{collections::BTreeMap, collections::BTreeSet, sync::Arc, sync::Condvar, sync::Mutex, sync::PoisonError, time::Duration, time::Instant, time::SystemTime};
use std::{ffi::OsStr, fs, fs::File, fs::OpenOptions, io::BufReader, io::Read, io::Write, path::Path, path::PathBuf, process, process::Command, process::Stdio};

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use crate::host::{self, Crash, Host, Launch, Live, Next, Phase, Protocol, Server, Slot};
use crate::manifest::Server as Declared;
use crate::results::{Item, Loc, Outcome, State, Store, Verdict, text};
use crate::rpc::{Cancel, Dialect, Framing, Handler, Peer, RpcError};
use crate::{env, parse, resolve, store, sys, sys::lock};

/// Diagnostics kept per server, over all its files; the rest are counted for `status`.
const MAX_DIAGS: usize = 2000;
/// Bytes kept of a diagnostic's or a log message's text.
const MAX_TEXT: usize = 512;
const MAX_FILE: u64 = 4 << 20;
const MAX_OPEN: usize = 50;
const MAX_SAVES: usize = 50;
const INIT: Duration = Duration::from_secs(120);
const QUERY: Duration = Duration::from_secs(30);
/// How long a nav query waits for the server to settle before it asks anyway.
const WAIT: Duration = Duration::from_secs(3);
/// How long an empty answer on a cold index that hasn't settled waits for it before asking again.
const COLD: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_millis(300);
const PUSH: Duration = Duration::from_millis(1500);
/// Fuzzy matches from `workspace/symbol` that may be a server's whole limit.
const CUT: usize = 100;
const NO_GIT: &str = "no git: unopened files not synced";

/// An answer, or the outcome that stands instead of it: an error, EMPTY, UNREADY or UNTRUSTED.
type Answer<T> = Result<T, Box<Outcome>>;

/// The client capabilities, the same for every server.
fn capabilities() -> Value {
    let kinds: Vec<u32> = (1..=26).collect();
    json!({
        "general": {"positionEncodings": ["utf-8", "utf-16"]}, "window": {"workDoneProgress": true, "showMessage": {}}, "experimental": {"serverStatusNotification": true},
        "workspace": {"configuration": true, "workspaceFolders": true, "didChangeWatchedFiles": {"dynamicRegistration": false},
            "diagnostics": {"refreshSupport": true}, "executeCommand": {}, "symbol": {"symbolKind": {"valueSet": kinds}}},
        "textDocument": {"synchronization": {"didSave": true}, "publishDiagnostics": {"relatedInformation": true, "versionSupport": true},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true, "symbolKind": {"valueSet": kinds}},
            "hover": {"contentFormat": ["plaintext", "markdown"]}, "definition": {"linkSupport": false}, "references": {}}
    })
}

/// What the reader keeps of an LSP server, reset at each start.
#[derive(Default)]
pub struct Seen {
    /// Pushed diagnostics by the server's URI.
    pub diags: BTreeMap<String, Diags>,
    /// Diagnostics kept over all URIs, at most `MAX_DIAGS`.
    pub kept: usize,
    /// Pushes so far; each `Diags` holds the one that set it.
    pub seq: u64,
    /// Progress by token: `None` once created, its last begin and report merged once begun.
    pub progress: BTreeMap<String, Option<Value>>,
    /// Methods the server registered dynamically.
    pub registered: BTreeSet<String>,
    pub quiescent: Option<bool>,
    /// `serverStatus` health other than `ok`, with its message: `error: Failed to load workspaces.`
    pub health: Option<String>,
    /// When the last message came, for the quiet a settle waits for.
    pub at: Option<Instant>,
    /// A settle finished since `initialize`: the index is no longer cold.
    pub warm: bool,
    /// The server counts columns in UTF-16 units, not bytes.
    pub utf16: bool,
    /// The server asked for the text with `didSave` (`textDocumentSync.save.includeText`).
    pub save_text: bool,
    /// The `capabilities` of its `initialize` answer.
    pub caps: Value,
    /// The files a sync or an open told it about, and when: a settle waits for a push for each,
    /// 1.5 s at most, whichever call told it.
    pub pending: BTreeMap<PathBuf, Instant>,
    /// Progress begins so far.
    pub begins: u64,
    /// A progress began right after a save: the server checks on save, as rust-analyzer runs
    /// `cargo check`, so a save with no check after it has not been heard.
    pub checks: bool,
    /// The last sync that saved files.
    pub saved: Option<Saved>,
}

/// The `didSave`s of a sync: when, the progress begins before them, and what to send again if
/// no check begins.
pub struct Saved {
    at: Instant,
    begins: u64,
    files: Vec<(String, PathBuf)>,
    resent: bool,
}

/// One URI's last push, in a compact form.
pub struct Diags {
    pub seq: u64,
    pub items: Vec<Diag>,
    /// Past `MAX_DIAGS`: counted, not kept.
    pub dropped: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    /// Start line and column, end line and column, in the server's units.
    pub range: [u32; 4],
    pub severity: u8,
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

impl Seen {
    fn push(&mut self, p: &Value) {
        let Some(uri) = p["uri"].as_str() else { return };
        self.seq += 1;
        self.kept -= self.diags.get(uri).map_or(0, |d| d.items.len());
        let all = p["diagnostics"].as_array().map_or(&[][..], Vec::as_slice);
        let items: Vec<Diag> = all.iter().take(MAX_DIAGS - self.kept).map(Diag::new).collect();
        self.kept += items.len();
        self.diags.insert(uri.into(), Diags { seq: self.seq, dropped: all.len() - items.len(), items });
        path_of(uri).inspect(|p| _ = self.pending.remove(p));
    }

    /// `cold` until the first settle after `initialize`, then `busy` while progress runs, else
    /// `warm`; with what the progress says, such as `Indexing 340/1200 (28%)`, and a health that
    /// isn't `ok`, since a server that failed to load its workspace answers warm and empty.
    pub fn index(&self) -> String {
        let mut running: Vec<String> = self.progress.values().flatten().map(progress_text).collect();
        let flag = match () {
            _ if !self.warm => "cold",
            _ if !running.is_empty() || self.quiescent == Some(false) => "busy",
            _ => "warm",
        };
        // Its first line only, clipped: `status` and the server log have the whole message.
        running.extend(self.health.as_ref().map(|h| format!("health {}", clip(h.lines().next().unwrap_or(""), 80))));
        if running.is_empty() { flag.into() } else { format!("{flag} ({})", running.join(", ")) }
    }

    /// What `status` shows of a server that is up, after its pid.
    pub fn summary(&self) -> Vec<String> {
        let dropped: usize = self.diags.values().map(|d| d.dropped).sum();
        let mut parts = vec![format!("index {}; {}", self.index(), parse::plural(self.kept, "diagnostic"))];
        parts.extend(self.health.as_ref().filter(|h| h.contains('\n') || h.len() > 80).map(|h| format!("health {}", h.replace('\n', " "))));
        parts.extend((dropped > 0).then(|| format!("{dropped} more past {MAX_DIAGS} counted, not kept")));
        parts.extend((!self.registered.is_empty()).then(|| format!("registered {}", self.registered.iter().cloned().collect::<Vec<_>>().join(", "))));
        parts
    }
}

impl Diag {
    fn new(d: &Value) -> Self {
        let (r, n) = (&d["range"], |v: &Value| v.as_u64().unwrap_or(0) as u32);
        let range = [n(&r["start"]["line"]), n(&r["start"]["character"]), n(&r["end"]["line"]), n(&r["end"]["character"])];
        let (code, source) = (d["code"].as_str().map(String::from).or_else(|| d["code"].as_i64().map(|c| c.to_string())), d["source"].as_str().map(String::from));
        Self { range, severity: d["severity"].as_u64().unwrap_or(1) as u8, code, source, message: clip(d["message"].as_str().unwrap_or(""), MAX_TEXT) }
    }
}

fn progress_text(v: &Value) -> String {
    let pct = v["percentage"].as_u64().map(|p| format!("({p}%)"));
    [v["title"].as_str().map(String::from), v["message"].as_str().map(String::from), pct].into_iter().flatten().collect::<Vec<_>>().join(" ")
}

/// At most `max` bytes, cut on a char boundary.
fn clip(s: &str, max: usize) -> String {
    s[..s.floor_char_boundary(max)].to_string()
}

/// The reader's side: answers the server's requests at once and keeps what it sends, touching only
/// the slot's `Live`, in short locks. A reader of an earlier start (`epoch`) records nothing.
pub struct Reader {
    pub live: Arc<Mutex<Live>>,
    pub woke: Arc<Condvar>,
    pub epoch: u64,
    pub settings: Value,
    pub folders: Value,
    /// The server log, which also gets its log and show messages.
    pub log: Option<Mutex<File>>,
}

impl Reader {
    fn message(&self, live: &mut Live, kind: &str, text: &str) {
        let line = format!("{} {kind}: {}", sys::utc(SystemTime::now()), clip(text, MAX_TEXT));
        let _ = self.log.as_ref().map(|log| writeln!(lock(log), "{line}"));
        live.last.push_back(line);
        live.last.drain(..live.last.len().saturating_sub(5));
    }
}

impl Handler for Reader {
    fn notify(&self, method: &str, p: Value) {
        let mut live = lock(&self.live);
        if live.epoch != self.epoch {
            return;
        }
        let seen = &mut live.lsp;
        seen.at = Some(Instant::now());
        let (token, v) = (p["token"].to_string(), &p["value"]);
        match method {
            "textDocument/publishDiagnostics" => seen.push(&p),
            "$/progress" if v["kind"] == "end" => _ = seen.progress.remove(&token),
            "$/progress" if v["kind"].is_string() => {
                if v["kind"] == "begin" {
                    seen.begins += 1;
                    seen.checks |= seen.saved.as_ref().is_some_and(|v| v.at.elapsed() < PUSH);
                }
                let merged = seen.progress.entry(token).or_default().get_or_insert_with(|| json!({}));
                v.as_object().into_iter().flatten().filter(|(k, _)| *k != "kind").for_each(|(k, x)| merged[k] = x.clone());
            }
            "experimental/serverStatus" => {
                let message = p["message"].as_str().filter(|m| !m.is_empty()).map_or(String::new(), |m| format!(": {}", clip(m.trim(), MAX_TEXT)));
                seen.quiescent = p["quiescent"].as_bool();
                let health = p["health"].as_str().filter(|h| *h != "ok").map(|h| format!("{h}{message}"));
                let new = std::mem::replace(&mut seen.health, health.clone()) != health;
                health.filter(|_| new).inspect(|h| self.message(&mut live, method, h));
            }
            "window/logMessage" | "window/showMessage" => self.message(&mut live, method, p["message"].as_str().unwrap_or("")),
            _ => {}
        }
        drop(live);
        self.woke.notify_all();
    }

    fn request(&self, method: &str, p: Value) -> Result<Value, (i64, String)> {
        let mut live = lock(&self.live);
        if live.epoch == self.epoch {
            live.lsp.at = Some(Instant::now());
            let methods = |key: &str| -> Vec<String> { p[key].as_array().into_iter().flatten().filter_map(|r| r["method"].as_str().map(String::from)).collect() };
            match method {
                "window/workDoneProgress/create" => _ = live.lsp.progress.entry(p["token"].to_string()).or_default(),
                "client/registerCapability" => live.lsp.registered.extend(methods("registrations")),
                // `unregisterations` is the spec's own spelling.
                "client/unregisterCapability" => methods("unregisterations").iter().for_each(|m| _ = live.lsp.registered.remove(m)),
                "window/showMessageRequest" => self.message(&mut live, method, p["message"].as_str().unwrap_or("")),
                _ => {}
            }
        }
        drop(live);
        self.woke.notify_all();
        match method {
            "workspace/configuration" => Ok(Value::Array(p["items"].as_array().into_iter().flatten().map(|i| section(&self.settings, i["section"].as_str())).collect())),
            "workspace/workspaceFolders" => Ok(self.folders.clone()),
            "window/showDocument" => Ok(json!({"success": false})),
            "workspace/applyEdit" => Ok(json!({"applied": false, "failureReason": "itse applies no edits"})),
            "window/workDoneProgress/create" | "client/registerCapability" | "client/unregisterCapability" | "window/showMessageRequest" => Ok(Value::Null),
            "workspace/diagnostic/refresh" | "workspace/semanticTokens/refresh" | "workspace/inlayHint/refresh" | "workspace/codeLens/refresh" => Ok(Value::Null),
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    fn closed(&self, _why: &str) {
        self.woke.notify_all();
    }

    fn skipped(&self, why: &str) {
        let mut live = lock(&self.live);
        if live.epoch == self.epoch {
            self.message(&mut live, "skipped", why);
        }
    }
}

/// `settings` at a dotted `section` (`rust-analyzer.cargo`), null where absent; no section is all of it.
fn section(settings: &Value, section: Option<&str>) -> Value {
    section.map_or(Some(settings), |s| s.split('.').try_fold(settings, |v, k| v.get(k))).cloned().unwrap_or(Value::Null)
}

/// `file://` and the path with every byte outside `[A-Za-z0-9/._~-]` percent-encoded.
fn uri(path: &Path) -> String {
    let keep = |b: u8| b.is_ascii_alphanumeric() || b"/-._~".contains(&b);
    let enc: String = path.as_os_str().as_bytes().iter().map(|&b| if keep(b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    format!("file://{enc}")
}

/// The path of a `file:` URI, however the server encoded it: `file:///p`, `file://localhost/p` or
/// `file:/p` (Java's `File.toURI`); always absolute.
pub fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file:")?;
    let rest = Some(rest.strip_prefix("//localhost").or_else(|| rest.strip_prefix("//")).unwrap_or(rest)).filter(|r| r.starts_with('/'))?;
    let (b, mut out, mut i) = (rest.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).filter(|_| b[i] == b'%').and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        out.push(hex.unwrap_or(b[i]));
        i += if hex.is_some() { 3 } else { 1 };
    }
    Some(PathBuf::from(OsStr::from_bytes(&out)))
}

/// A server's column on `line` (UTF-16 units, else bytes) as a byte offset into it.
fn byte_col(line: &str, col: u32, utf16: bool) -> usize {
    if !utf16 {
        return line.floor_char_boundary(col as usize);
    }
    let mut units = 0;
    for (i, c) in line.char_indices() {
        if units >= col {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

/// A byte offset into `line` as the server's column: UTF-16 units, else bytes.
fn server_col(line: &str, byte: usize, utf16: bool) -> usize {
    let byte = line.floor_char_boundary(byte);
    if utf16 { line[..byte].encode_utf16().count() } else { byte }
}

/// What a server was told of the disk: the documents opened for queries, the least recently used
/// first, and the last `git status`. Of the start `epoch`: a query still holding an earlier
/// start's peer changes nothing here.
#[derive(Default)]
pub struct Docs {
    pub epoch: u64,
    open: Vec<Doc>,
    git: Option<Git>,
}

struct Doc {
    path: PathBuf,
    uri: String,
    version: i64,
    stamp: Stamp,
}

struct Git {
    head: String,
    /// The changed files git listed, of the server's filetypes, with their mtimes.
    seen: BTreeMap<PathBuf, i128>,
}

/// (mtime in ns, size, inode): an edit, a truncation or a replace-by-rename each changes one.
type Stamp = (i128, u64, u64);

/// A regular file's only: a link to a device or a FIFO is no file a server is told about.
fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok().filter(fs::Metadata::is_file)?;
    Some((m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128, m.size(), m.ino()))
}

/// A file's text for the server: refused past 4 MB, invalid UTF-8 replaced. A regular file only,
/// opened without blocking, so a FIFO swapped in hangs nothing, and read through the cap, so a file
/// that grows meanwhile can't get past it.
fn read(path: &Path) -> Result<String> {
    let (file, mut bytes) = (OpenOptions::new().read(true).custom_flags(sys::O_NONBLOCK).open(path)?, Vec::new());
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "no file {}", path.display());
    ensure!(meta.len() <= MAX_FILE, "file_too_large: {} is {} bytes, over 4 MB", path.display(), meta.len());
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "file_too_large: {} grew past 4 MB", path.display());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The paths a git command that succeeds lists, NUL-separated, absolute; each entry past its first `skip` bytes.
fn git_paths(root: &Path, args: &[&str], skip: usize) -> Option<Vec<PathBuf>> {
    let out = sys::git(root).args(args).output().ok().filter(|o| o.status.success())?;
    Some(out.stdout.split(|&b| b == 0).filter_map(|e| e.get(skip..)).filter(|p| !p.is_empty()).map(|p| root.join(OsStr::from_bytes(p))).collect())
}

/// `didSave`, with the text when the server asked for it: the one just sent, else read now (not past 4 MB).
fn save(peer: &Peer, uri: &str, path: &Path, text: Option<&str>, with_text: bool) {
    let mut params = json!({"textDocument": {"uri": uri}});
    if with_text && let Some(text) = text.map(String::from).or_else(|| read(path).ok()) {
        params["text"] = text.into();
    }
    let _ = peer.notify("textDocument/didSave", params);
}

fn close(peer: &Peer, d: &Doc) -> bool {
    let _ = peer.notify("textDocument/didClose", json!({"textDocument": {"uri": d.uri}}));
    false
}

/// One LSP server, up, for one query.
pub struct Ask {
    pub handle: String,
    pub peer: Arc<Peer>,
    pub slot: Arc<Slot>,
    pub root: PathBuf,
    pub git: bool,
    pub launch: Launch,
    /// Why it was started now, a config change, what the sync couldn't do.
    pub notes: Vec<String>,
    /// Held for the query, so the idle stop leaves the server alone meanwhile.
    pub _held: Option<Arc<Server>>,
    /// This call started the server, or waited for its start.
    pub fresh: bool,
    /// The server's start, as `Live::epoch`.
    pub epoch: u64,
}

/// A query's end counts as a use, so the idle time runs from there.
impl Drop for Ask {
    fn drop(&mut self) {
        lock(&self.slot.live).used = Instant::now();
    }
}

impl Ask {
    /// Tell the server what changed on disk since the last query: an opened document that changed
    /// gets its full text (`didChange`, version + 1) and `didSave`, a deleted one `didClose`; of the
    /// unopened files git lists as changed, those of the server's filetypes with a new mtime are
    /// opened from disk and saved (50 at most, the newest first), and so are those that differ
    /// between the old and the new HEAD when it moved. A save alone may go unheard for a file the
    /// server doesn't hold, and rust-analyzer then runs no check. Even a checkout that leaves the
    /// tree clean gets one `didSave` (of an opened document, if no file of its filetypes changed),
    /// since rust-analyzer reruns its check only on a save. With `watch: client` the server also
    /// gets `didChangeWatchedFiles` for them. The files it told about, which `settle` waits for.
    pub fn sync(&mut self) -> Vec<PathBuf> {
        // Live is never held while waiting for another lock: the reader and `status` need it.
        let (mut docs, mut told, mut saves, at) = (lock(&self.slot.docs), Vec::new(), Vec::new(), Instant::now());
        let live = lock(&self.slot.live);
        let (since, begins, with_text) = (live.lsp.seq, live.lsp.begins, live.lsp.save_text);
        drop(live);
        if docs.epoch != self.epoch {
            return told;
        }
        let peer = &self.peer;
        docs.open.retain_mut(|d| {
            let (now, text) = match stamp(&d.path) {
                Some(s) if s == d.stamp => return true,
                now => (now, now.and_then(|_| read(&d.path).ok())),
            };
            told.push(d.path.clone());
            let (Some(now), Some(text)) = (now, text) else { return close(peer, d) };
            (d.version, d.stamp) = (d.version + 1, now);
            let _ = peer.notify("textDocument/didChange", json!({"textDocument": {"uri": d.uri, "version": d.version}, "contentChanges": [{"text": text}]}));
            save(peer, &d.uri, &d.path, Some(&text), with_text);
            saves.push((d.uri.clone(), d.path.clone()));
            true
        });
        // Without renames, each entry is `XY path`; a submodule's own status (its own config) never runs.
        let status = ["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", "--ignore-submodules=all"];
        match self.git.then(|| git_paths(&self.root, &status, 3)).flatten() {
            None => self.notes.push(NO_GIT.into()),
            Some(files) => {
                // HEAD is empty before the first commit.
                let head =
                    sys::git(&self.root).args(["rev-parse", "-q", "--verify", "HEAD"]).output().map_or(String::new(), |o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                let opened: BTreeSet<PathBuf> = docs.open.iter().map(|d| d.path.clone()).collect();
                let ours = |p: &PathBuf| !opened.contains(p) && p.extension().and_then(OsStr::to_str).is_some_and(|e| self.launch.filetypes.iter().any(|f| f == e));
                let mtime = |p: &PathBuf| stamp(p).map(|s| s.0);
                let seen: BTreeMap<PathBuf, i128> = files.into_iter().filter(ours).filter_map(|p| Some((p.clone(), mtime(&p)?))).collect();
                // The first status after a start only notes what is there: the server has just read the disk.
                let (mut fresh, mut gone) = (vec![], vec![]);
                if let Some(old) = &docs.git {
                    // A file git no longer lists (reverted, stashed) changed too, or is gone.
                    let reverted: Vec<&PathBuf> = old.seen.keys().filter(|p| !seen.contains_key(*p) && !opened.contains(*p)).collect();
                    gone = reverted.iter().filter(|p| !p.exists()).map(|p| json!({"uri": uri(p), "type": 3})).collect();
                    // Names only, with no external diff or textconv the repo's config could name; none while either HEAD is unknown.
                    let diff = ["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", &old.head, &head];
                    let moved = if old.head != head && !old.head.is_empty() && !head.is_empty() { git_paths(&self.root, &diff, 0).unwrap_or_default() } else { vec![] };
                    let changed = |p: &PathBuf| mtime(p).filter(|m| old.seen.get(p) != Some(m));
                    fresh = seen.keys().chain(reverted).filter_map(|p| Some((changed(p)?, p.clone()))).collect();
                    fresh.extend(moved.into_iter().filter(ours).filter_map(|p| Some((mtime(&p)?, p))));
                    fresh.sort_unstable_by(|a, b| b.cmp(a));
                    fresh.dedup_by(|a, b| a.1 == b.1);
                    fresh.truncate(MAX_SAVES);
                    if old.head != head && told.is_empty() && fresh.is_empty() {
                        fresh.extend(docs.open.last().map(|d| (0, d.path.clone())));
                    }
                }
                docs.git = Some(Git { head, seen });
                for (_, p) in fresh {
                    // An opened document (the checkout's one save) is only moved to the end.
                    let uri = self.open_in(&mut docs, &p).unwrap_or_else(|_| uri(&p));
                    save(peer, &uri, &p, None, with_text);
                    saves.push((uri, p.clone()));
                    told.push(p);
                }
                let changes: Vec<Value> = saves.iter().map(|(u, _)| json!({"uri": u, "type": 2})).chain(gone).collect();
                if self.launch.watch_client && !changes.is_empty() {
                    let _ = peer.notify("workspace/didChangeWatchedFiles", json!({"changes": changes}));
                }
            }
        }
        let s = &mut lock(&self.slot.live).lsp;
        let unpushed = told.iter().filter(|p| !s.diags.iter().any(|(u, d)| d.seq > since && path_of(u).as_ref() == Some(*p)));
        s.pending.extend(unpushed.map(|p| (p.clone(), at)));
        if !saves.is_empty() {
            s.saved = Some(Saved { at, begins, files: saves, resent: false });
        }
        told
    }

    /// The document at `path` opened in the server (`didOpen`, version 1), or moved to the end of
    /// the 50 kept if it already is; the oldest one is closed past 50. Its URI.
    pub fn open(&self, path: &Path) -> Result<String> {
        let mut docs = lock(&self.slot.docs);
        ensure!(docs.epoch == self.epoch, "server_crash: {} started again during the call; the next call uses it", self.handle);
        self.open_in(&mut docs, path)
    }

    /// `open`, with the documents held; a new one waits for its push like a synced file.
    fn open_in(&self, docs: &mut Docs, path: &Path) -> Result<String> {
        if let Some(i) = docs.open.iter().position(|d| d.path == path) {
            docs.open[i..].rotate_left(1);
            return Ok(docs.open[docs.open.len() - 1].uri.clone());
        }
        let (stamp, text) = (stamp(path).ok_or_else(|| anyhow!("no file {}", path.display()))?, read(path)?);
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or_default();
        let (uri, lang) = (uri(path), self.launch.languages.get(ext).map_or(ext, String::as_str));
        self.peer.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": lang, "version": 1, "text": text}}))?;
        lock(&self.slot.live).lsp.pending.insert(path.into(), Instant::now());
        docs.open.push(Doc { path: path.into(), uri: uri.clone(), version: 1, stamp });
        if docs.open.len() > MAX_OPEN {
            close(&self.peer, &docs.open.remove(0));
        }
        Ok(uri)
    }

    /// Wait until the server has caught up, at most until `deadline`: no progress running,
    /// `serverStatus` not saying it is busy, 300 ms without a message, a push for each file told
    /// about (or 1.5 s), and with a server that checks on save, a check begun since the last save.
    /// With none 1.5 s after it, the saves go once more; with none after that either, the server
    /// hasn't heard of the edits and every settle is false until a check or the next save. True if
    /// it caught up; the first settle after `initialize` makes the index warm. A cancelled call, or
    /// one whose server started again meanwhile, stops waiting, unsettled.
    pub fn settle(&self, deadline: Instant, cancel: &Cancel) -> bool {
        let mut live = lock(&self.slot.live);
        while live.epoch == self.epoch {
            let (now, s) = (Instant::now(), &mut live.lsp);
            s.pending.retain(|_, at| now.duration_since(*at) < PUSH);
            s.checks |= s.saved.as_ref().is_some_and(|v| s.begins > v.begins && now.duration_since(v.at) < 2 * PUSH);
            let busy = s.progress.values().any(Option::is_some) || s.quiescent == Some(false);
            let quiet = s.at.is_none_or(|at| now.duration_since(at) >= QUIET);
            let (checks, begins, with_text) = (s.checks, s.begins, s.save_text);
            let unheard = s.saved.as_mut().filter(|v| checks && v.begins == begins);
            if !busy && quiet && s.pending.is_empty() && unheard.is_none() {
                s.warm = true;
                return true;
            }
            match unheard.filter(|v| now.duration_since(v.at) >= PUSH) {
                Some(v) if v.resent => return false,
                Some(v) => {
                    (v.resent, v.at) = (true, now);
                    let files = v.files.clone();
                    drop(live);
                    files.iter().for_each(|(uri, path)| save(&self.peer, uri, path, None, with_text));
                    live = lock(&self.slot.live);
                }
                None if now >= deadline || self.peer.closed().is_some() || cancel.is_cancelled() => return false,
                None => live = self.slot.woke.wait_timeout(live, (deadline - now).min(Duration::from_millis(50))).unwrap_or_else(PoisonError::into_inner).0,
            }
        }
        false
    }

    /// The index flag, `cold` for the call that started the server whatever its first settle says:
    /// a server that has just loaded may still be filling its caches.
    pub fn index(&self) -> String {
        let live = lock(&self.slot.live);
        let index = if live.epoch == self.epoch { live.lsp.index() } else { "cold".into() };
        match index.strip_prefix("warm") {
            Some(rest) if self.fresh => format!("cold{rest}"),
            _ => index,
        }
    }

    /// `rust-analyzer index warm`, for an answer's counts.
    fn indexed(&self) -> String {
        format!("{} index {}", self.launch.id, self.index())
    }

    /// Sync, then settle for up to `wait`; one that doesn't is noted, so the answer may be behind the disk.
    pub fn catch_up(&mut self, wait: Duration, cancel: &Cancel) -> bool {
        self.sync();
        let settled = self.settle(Instant::now() + wait, cancel);
        self.notes.extend((!settled).then(|| format!("{} not settled after {} s: the answer may miss the latest edits", self.launch.id, wait.as_secs())));
        settled
    }

    /// A query, 30 s at most; a server that ends under it is `server_crash`.
    pub fn request(&self, method: &str, params: Value, cancel: &Cancel) -> Result<Value> {
        self.peer.request(method, params, QUERY, cancel).map_err(|e| match e {
            RpcError::Closed(why) => anyhow!("server_crash: {} ended during the call ({why}); the next call starts it again", self.handle),
            RpcError::Timeout => anyhow!("{}: timeout: no answer in {} s (index busy?); itse status shows its progress", self.handle, QUERY.as_secs()),
            e => anyhow!("{}: {e}", self.handle),
        })
    }

    /// The server checks on save and began no check after the last saves, sent twice.
    fn unheard(&self) -> bool {
        let s = &lock(&self.slot.live).lsp;
        s.checks && s.saved.as_ref().is_some_and(|v| v.resent && v.begins == s.begins)
    }

    /// Whether the server's `initialize` answer offers `cap`, such as `definitionProvider`.
    fn offers(&self, cap: &str) -> bool {
        let c = &lock(&self.slot.live).lsp.caps[cap];
        *c == true || c.is_object()
    }

    fn symbols(&self, query: &str, cancel: &Cancel) -> Result<Vec<Value>> {
        Ok(self.request("workspace/symbol", json!({"query": query}), cancel)?.as_array().cloned().unwrap_or_default())
    }

    /// A path as the repo's, if it is in the workspace.
    fn rel(&self, path: PathBuf) -> PathBuf {
        path.strip_prefix(&self.root).map(Path::to_path_buf).unwrap_or(path)
    }

    /// A symbol's repo path, from its location.
    fn rel_of(&self, s: &Value) -> PathBuf {
        self.rel(path_of(s["location"]["uri"].as_str().unwrap_or("")).unwrap_or_default())
    }

    /// The symbols named `last` (of `kinds`, if given) whose qualifiers fit, else those whose name
    /// starts with it in any case. An item of an inline module has no container either: its
    /// file's outline names the modules around it, which opens that file in the server.
    fn matches(&self, all: &[Value], last: &str, quals: &[&str], kinds: &[u64], cancel: &Cancel) -> Vec<Value> {
        let is = |n: &str, exact: bool| if exact { n == last } else { n.to_lowercase().starts_with(&last.to_lowercase()) };
        // Asked for methods (6 and 12), a function without a container is a free one.
        let kind = |s: &Value| s["kind"].as_u64().is_some_and(|k| kinds.contains(&k) && !(k == 12 && kinds.contains(&6) && container(s).is_empty()));
        let named = |s: &Value, exact| s["name"].as_str().is_some_and(|n| is(n, exact)) && (kinds.is_empty() || kind(s));
        let fits = |s: &&Value, exact| named(s, exact) && qualified(quals, &container(s), &self.rel_of(s));
        let hits = [true, false].into_iter().map(|exact| all.iter().filter(|s| fits(s, exact)).cloned().collect::<Vec<_>>()).find(|h| !h.is_empty());
        if hits.is_some() || quals.is_empty() {
            return hits.unwrap_or_default();
        }
        let bare = all.iter().filter(|s| named(s, true) && container(s).is_empty()).take(5);
        bare.filter(|s| Some(self.modules(s, cancel)).filter(|m| !m.is_empty()).is_some_and(|m| qualified(quals, &m, &self.rel_of(s)))).cloned().collect()
    }

    /// The modules around a symbol, outermost first, from its file's `documentSymbol` tree.
    fn modules(&self, s: &Value, cancel: &Cancel) -> Vec<String> {
        let outline = |uri: String| self.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri}}), cancel);
        let tree = path_of(s["location"]["uri"].as_str().unwrap_or("")).and_then(|path| self.open(&path).and_then(outline).ok());
        let at = |p: &Value| (p["line"].as_u64(), p["character"].as_u64());
        let (pos, mut mods, mut level) = (at(&s["location"]["range"]["start"]), vec![], tree.unwrap_or_default());
        let around = |m: &&Value| matches!(m["kind"].as_u64(), Some(2 | 3)) && at(&m["range"]["start"]) <= pos && pos <= at(&m["range"]["end"]);
        while let Some(m) = level.as_array().into_iter().flatten().find(around) {
            mods.push(m["name"].as_str().unwrap_or("").to_string());
            level = m["children"].clone();
        }
        mods
    }

    /// A location the server sent, as an item: repo-relative, 1-based, in bytes, with its source line.
    pub fn item(&self, id: String, kind: &str, loc: &Value, msg: String) -> Item {
        let (uri, start) = (loc["uri"].as_str().unwrap_or(""), &loc["range"]["start"]);
        let (line, col) = (start["line"].as_u64().unwrap_or(0) as u32 + 1, start["character"].as_u64().unwrap_or(0) as u32);
        // A library source in a jar (`jar:`, `jdt://`) has no file: its URI is shown as it is.
        let Some(path) = path_of(uri) else { return Item { id, kind: kind.into(), loc: Some(Loc { path: uri.into(), line, col: col + 1 }), msg, ..Item::default() } };
        let rel = self.rel(path.clone());
        let src = parse::source(&self.root, &rel.to_string_lossy(), line);
        let col = src.as_ref().map_or(col, |(_, text)| byte_col(text, col, lock(&self.slot.live).lsp.utf16) as u32) + 1;
        let shown = match std::env::var_os("HOME").and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
            Some(under) if !path.starts_with(&self.root) => format!("~/{}", under.display()),
            _ => rel.display().to_string(),
        };
        Item { id, kind: kind.into(), loc: Some(Loc { path: shown, line, col }), src: src.into_iter().collect(), msg, ..Item::default() }
    }
}

/// The launch's hash, as `Server::hash`.
fn launch_hash(l: &Launch) -> String {
    sys::sha256_hex(format!("{l:?}").as_bytes())[..12].to_string()
}

/// A server left running from a launch that has changed since.
fn changed(s: &Server, l: &Launch) -> Option<String> {
    (s.hash != launch_hash(l)).then(|| format!("config changed; 'itse stop lsp:{}' to apply", l.id))
}

/// `lsp:<id>` up for a query, started on demand by the first caller while the others wait for it,
/// unless `start` is false: then None if it isn't up. One found ended leaves `procs.json` here.
fn ask(host: &Arc<Host>, l: &Launch, env: &BTreeMap<String, String>, start: bool, cancel: &Cancel) -> Result<Option<Ask>> {
    let handle = format!("lsp:{}", l.id);
    let slot = host.slot(&handle);
    let (up, ended) = slot.get();
    ended.inspect(|&pid| host.untrack(&handle, pid));
    let (server, note, fresh) = match up {
        Some(server) => (server, None, false),
        None if !start => return Ok(None),
        None => slot.up(|| self::start(host, &slot, l, env), cancel)?,
    };
    let (root, git) = (host.ws.root.clone(), host.ws.git);
    // A declared output the server makes when it is missing, such as rust-analyzer's Cargo.lock.
    let makes = l.outputs.iter().filter(|o| !o.ends_with('/') && !root.join(o).exists()).map(|o| format!("{} may write {o} into the workspace (a declared output)", l.id));
    let notes = note.map(|n| format!("{} {n}", l.id)).into_iter().chain(changed(&server, l)).chain(makes).collect();
    Ok(Some(Ask { handle, peer: server.peer.clone(), slot, root, git, launch: l.clone(), notes, epoch: server.epoch, _held: Some(server), fresh }))
}

/// Spawn the server in its own group, stderr to `servers/<id>.log` (the last start's kept as
/// `<id>.log.1`, so a crash's tail is this start's; the daemon starts it afresh past 4 MB), and
/// `initialize` it. It is in `procs.json` from the spawn on, with the `ITSE_SERVER` marker its env
/// passes down to what it starts, so a daemon that dies during a long `initialize` leaves nothing
/// behind; the marker is the one `ITSE_*` var a server gets. A server that ends or fails before
/// `initialize` answers has its group and its marked processes killed, and says how.
fn start(host: &Host, slot: &Slot, l: &Launch, env: &BTreeMap<String, String>) -> Result<Server> {
    let (program, args) = l.argv.split_first().ok_or_else(|| anyhow!("{} has no command", l.id))?;
    let (dir, handle) = (host.ws.state_dir(&host.dirs).join("servers"), format!("lsp:{}", l.id));
    sys::private_dir(&dir)?;
    let log = dir.join(format!("{}.log", l.id));
    let _ = fs::rename(&log, dir.join(format!("{}.log.1", l.id)));
    let marker = format!("{}-{}", process::id(), sys::hex(&sys::random_bytes::<8>()?));
    let file = OpenOptions::new().create(true).append(true).mode(0o600).open(&log)?;
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(&l.cwd).env_clear().envs(env).envs(l.env.iter().cloned()).env(host::MARKER, &marker);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(file.try_clone()?);
    let mut child = host::spawn(&mut cmd, &l.limits, false, 800).map_err(|e| anyhow!("starting {program}: {e}"))?;
    // Only the one start under way changes the epoch.
    let epoch = lock(&slot.live).epoch + 1;
    *lock(&slot.live) = Live { epoch, ..Live::default() };
    *lock(&slot.docs) = Docs { epoch, ..Docs::default() };
    let folders = json!([{"uri": uri(&host.ws.root), "name": host.ws.root.file_name().unwrap_or_default().to_string_lossy()}]);
    let reader = Reader { live: slot.live.clone(), woke: slot.woke.clone(), epoch, settings: l.settings.clone(), folders: folders.clone(), log: Some(Mutex::new(file)) };
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else { bail!("{program} has no stdio") };
    let peer = Peer::start(BufReader::new(stdout), stdin, Framing::Headers, Dialect::Lsp, Arc::new(reader));
    let secs = std::env::var("ITSE_SERVER_IDLE_S").ok().and_then(|s| s.parse().ok());
    let idle = Duration::from_secs(secs.unwrap_or_else(|| l.idle_minutes.or(host.config().server_idle_minutes).unwrap_or(30) * 60));
    let (pid, utc) = (child.id() as i32, sys::utc(SystemTime::now()));
    let start_time = sys::proc_stat(pid).map_or(0, |s| s.start_time);
    let record = json!({"handle": handle, "kind": "lsp", "pid": pid, "pgid": pid, "start_time": start_time, "argv0": program, "started_at": utc, "log": log, "marker": marker});
    host.track(&handle, Some(record));
    let server = Server { peer, child: Mutex::new(child), exit_codes: l.exit_codes.clone(), log, idle, hash: launch_hash(l), marker, epoch };
    if let Err(e) = initialize(&server.peer, slot, l, &host.ws.root, folders) {
        let mut child = lock(&server.child);
        host::kill_tree(pid, &server.marker, || drop(child.try_wait()));
        host.untrack(&handle, pid);
        let ended = child.wait().map(|s| Crash::new(s, &l.exit_codes, &server.log));
        let tail = ended.as_ref().ok().and_then(|c| c.stderr_tail.last().map(|t| format!("; log: {t}"))).unwrap_or_default();
        bail!("{} before initialize answered ({e}){tail}", ended.map_or("killed".into(), |c| c.text));
    }
    Ok(server)
}

/// `initialize` (processId, the root as rootUri and the one workspace folder, clientInfo, the
/// server's init options), then `initialized` and the settings. The server picks the position
/// encoding; one that names none counts in UTF-16, as the spec says. Once it has answered, it has
/// started: one that ends before the notifications are written is a crash the next request sees.
fn initialize(peer: &Peer, slot: &Slot, l: &Launch, root: &Path, folders: Value) -> Result<()> {
    let params = json!({"processId": process::id(), "rootUri": uri(root), "workspaceFolders": folders, "clientInfo": {"name": "itse", "version": env!("CARGO_PKG_VERSION")},
        "capabilities": capabilities(), "initializationOptions": l.init_options});
    let reply = peer.request("initialize", params, INIT, &Cancel::default()).map_err(|e| anyhow!("initialize: {e}"))?;
    {
        let (s, caps) = (&mut lock(&slot.live).lsp, &reply["capabilities"]);
        (s.utf16, s.at, s.save_text) = (caps["positionEncoding"].as_str() != Some("utf-8"), Some(Instant::now()), caps["textDocumentSync"]["save"]["includeText"] == true);
        s.caps = caps.clone();
    }
    let _ = peer.notify("initialized", json!({}));
    let _ = peer.notify("workspace/didChangeConfiguration", json!({"settings": l.settings}));
    Ok(())
}

/// The LSP servers of a call: those asked, each up for a query, and those in scope that weren't.
#[derive(Default)]
pub struct Asks {
    pub asks: Vec<Ask>,
    /// Not started before trust, since they run project code.
    pub untrusted: Vec<String>,
    /// What is missing: before trust the `untrusted` row, which says who acts next, else the rows
    /// of what isn't ready.
    pub states: Vec<State>,
    /// Not running, on a call that starts none.
    pub idle: Vec<String>,
    /// In scope but not ready (a package missing, a path install gone), so not asked.
    pub unready: Vec<String>,
}

impl Asks {
    /// For a query that needs a server: UNREADY when none in scope is ready, with what is missing.
    fn ready(self, verb: &str, scope: &str) -> Answer<Self> {
        if !self.asks.is_empty() || self.unready.is_empty() {
            return Ok(self);
        }
        let counts = format!("{} not ready", self.unready.join(", "));
        Err(Box::new(Outcome { states: self.states, err: Some("not_installed"), ..Outcome::new(verb, Verdict::Unready, scope, counts) }))
    }
}

/// The LSP servers in use whose filetypes `serves` takes, each up for a query, started on demand
/// unless `start` is false (then those not running are `idle`): resolved again for every call, so a
/// manifest edit applies at the next start. A server held for trust (it or its package runs project
/// code, or it comes from the team layer) is never started in an untrusted workspace: with none
/// other in scope, that is UNTRUSTED with `itse trust` for a human, before anything spawns. One
/// left running that may no longer start (a revoked trust, an unused extension, a manifest edit)
/// is stopped.
pub fn servers(host: &Arc<Host>, caller: &BTreeMap<String, String>, verb: &str, scope: &str, serves: &dyn Fn(&[String]) -> bool, start: bool, cancel: &Cancel) -> Answer<Asks> {
    let fail = |err: &'static str, e: String| Box::new(Outcome::error(verb, err, e));
    let r = resolve::resolve_for(&host.ws, &host.dirs, caller, SystemTime::now(), false).map_err(|e| fail("bad_args", format!("{e:#}")))?;
    let all: Vec<&Declared> = r.manifest.uses().iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| &e.0.servers).filter(|s| s.protocol == Protocol::Lsp).collect();
    let resolved = |id: &str| r.launches.iter().any(|l| l.protocol == Protocol::Lsp && l.id == id);
    let left: Vec<String> = lock(&host.slots).keys().filter(|h| h.strip_prefix("lsp:").is_some_and(|id| !resolved(id))).cloned().collect();
    left.iter().for_each(|h| _ = host.stop_server(h, Duration::from_secs(5)));
    let scoped: Vec<&Declared> = all.iter().copied().filter(|s| serves(&s.filetypes)).collect();
    let untrusted: Vec<String> = scoped.iter().filter(|s| r.held.contains(&s.id)).map(|s| s.id.clone()).collect();
    let missing = |s: &&State| if untrusted.is_empty() { !matches!(s.state.as_str(), "ready" | "unpinned" | "expiring" | "suggested") } else { s.state == "untrusted" };
    let states: Vec<State> = r.states.iter().filter(missing).cloned().collect();
    let launches: Vec<&Launch> = r.launches.iter().filter(|l| l.protocol == Protocol::Lsp && serves(&l.filetypes)).collect();
    let unready: Vec<String> = scoped.iter().filter(|s| !resolved(&s.id) && !untrusted.contains(&s.id)).map(|s| s.id.clone()).collect();
    if launches.is_empty() {
        let held = format!("{} runs project code", untrusted.join(", "));
        return match () {
            _ if all.is_empty() => Err(Box::new(Outcome::new(verb, Verdict::Empty, scope, "no language server in use"))),
            _ if !untrusted.is_empty() => Err(Box::new(Outcome { states, err: Some("untrusted"), ..Outcome::new(verb, Verdict::Untrusted, scope, held) })),
            // What is missing, if anything, which `ready` makes the answer of a query that needs a server.
            _ => Ok(Asks { states, unready, ..Asks::default() }),
        };
    }
    let base: BTreeMap<String, String> = host.base_env.iter().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect();
    let env = env::workspace_env(&r, &host.dirs, base.clone(), &store::Real(base)).map_err(|e| fail("io", format!("{e:#}")))?;
    let mut found = Asks { untrusted, states, unready, ..Asks::default() };
    for l in launches {
        match ask(host, l, &env, start, cancel).map_err(|e| fail("server_start", format!("lsp:{}: {e:#}", l.id)))? {
            Some(ask) => found.asks.push(ask),
            None => found.idle.push(l.id.clone()),
        }
    }
    if let Some(first) = found.asks.first_mut() {
        first.notes.extend((!found.untrusted.is_empty()).then(|| format!("{} not started: runs project code; next (human): itse trust", found.untrusted.join(", "))));
    }
    Ok(found)
}

/// The `status` row of `lsp:<id>`, read without waiting on a start: `starting (41 s)`, `up` with
/// its index flag, `crashed` with how, `gone` after a daemon restart, or `stopped`.
pub fn row(host: &Host, l: &Launch) -> State {
    let handle = format!("lsp:{}", l.id);
    let slot = lock(&host.slots).get(&handle).cloned().unwrap_or_default();
    let (state, reason) = match slot.phase.try_lock().as_deref() {
        Ok(Phase::Starting(at)) => (format!("starting ({} s)", at.elapsed().as_secs()), String::new()),
        Ok(Phase::Up(s)) => {
            let live = lock(&slot.live);
            let pid = s.child.try_lock().map(|c| format!("pid {}", c.id()));
            let parts: Vec<String> = pid.into_iter().chain(live.lsp.summary()).chain(changed(s, l)).chain(live.last.back().cloned()).collect();
            ("up".into(), parts.join("; "))
        }
        Ok(Phase::Crashed(c)) => ("crashed".into(), format!("{}{}; the next use starts it again", c.text, c.stderr_tail.last().map_or(String::new(), |t| format!("; log: {t}")))),
        Ok(Phase::Gone(why)) => ("gone".into(), format!("{why}; the next use starts it cold")),
        Ok(Phase::Stopped) => ("stopped".into(), "starts on first use".into()),
        Err(_) => ("busy".into(), String::new()),
    };
    State { item: handle, state, reason: Some(reason).filter(|r| !r.is_empty()), ..State::default() }
}

/// The usage log's class of a failed query.
fn err_class(e: &anyhow::Error) -> &'static str {
    match e.to_string() {
        s if s.starts_with("server_crash") => "server_crash",
        s if s.starts_with("file_too_large") => "file_too_large",
        s if s.contains(": timeout") => "timeout",
        s if s.ends_with("cancelled") => "cancel",
        _ => "io",
    }
}

/// A failed query as the verb's error, with its class.
fn failed(verb: &str, e: anyhow::Error) -> Box<Outcome> {
    Box::new(Outcome::error(verb, err_class(&e), format!("{e:#}")))
}

/// LSP's SymbolKind names, 1 to 26.
#[rustfmt::skip]
const KINDS: [&str; 26] = ["file", "module", "namespace", "package", "class", "method", "property", "field", "constructor", "enum", "interface", "function", "variable",
    "constant", "string", "number", "boolean", "array", "object", "key", "null", "enum-member", "struct", "event", "operator", "type-parameter"];

fn kind(symbol: &Value) -> &'static str {
    symbol["kind"].as_u64().and_then(|k| KINDS.get(k.checked_sub(1)? as usize)).unwrap_or(&"symbol")
}

/// The SymbolKinds `--kind K` takes: `method` is 6, and 12 inside a container, since
/// rust-analyzer reports methods as functions of their impl's type.
fn kinds(kind: Option<&str>) -> Result<Vec<u64>, String> {
    match kind {
        None => Ok(vec![]),
        Some("method") => Ok(vec![6, 12]),
        Some(k) => KINDS.iter().position(|n| *n == k).map(|i| vec![i as u64 + 1]).ok_or_else(|| format!("unknown kind {k}; one of {}", KINDS.join(", "))),
    }
}

/// The edits that turn `a` into `b`, for ranking the nearest names.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = std::mem::replace(&mut row[0], i + 1);
        for (j, cb) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = (up + 1).min(row[j] + 1).min(diag + usize::from(ca != *cb));
            diag = up;
        }
    }
    row[b.len()]
}

/// A name's segments, split on `.`, `::` and `#`: `data::Region.save` is `[data, Region, save]`.
fn segments(name: &str) -> Vec<&str> {
    name.split("::").flat_map(|s| s.split(['.', '#'])).filter(|s| !s.is_empty()).collect()
}

/// A symbol's `containerName` as segments, without generic arguments: rust-analyzer's `Wrapper<T>`
/// is `[Wrapper]`, and `Foo<std::io::Error>` is `[Foo]`.
fn container(s: &Value) -> Vec<String> {
    let mut depth = 0;
    let mut bare = |c: char| {
        depth = (depth + i32::from(c == '<') - i32::from(c == '>')).max(0);
        depth == 0 && c != '>'
    };
    let bare: String = s["containerName"].as_str().unwrap_or("").chars().filter(|&c| bare(c)).collect();
    segments(&bare).into_iter().map(String::from).collect()
}

/// Whether a symbol with these container segments, at repo path `rel`, fits the qualifiers before
/// its name: the last is the container's last segment, or with no container (rust-analyzer gives
/// only an impl's self type, and none to a free function) a path component or the file stem, in
/// any case; each earlier one is a segment of the container or a path component.
fn qualified(quals: &[&str], container: &[String], rel: &Path) -> bool {
    let parts: Vec<String> = rel.iter().chain(rel.file_stem()).map(|p| p.to_string_lossy().to_lowercase()).collect();
    let in_path = |q: &str| parts.contains(&q.to_lowercase());
    let Some((last, earlier)) = quals.split_last() else { return true };
    container.last().map_or_else(|| in_path(last), |c| c == last) && earlier.iter().all(|q| container.iter().any(|c| c == q) || in_path(q))
}

/// A symbol's item id: `S` and its place in the result.
fn sid(n: usize) -> String {
    format!("S{n}")
}

fn qualified_name(s: &Value) -> String {
    let name = s["name"].as_str().unwrap_or("");
    s["containerName"].as_str().filter(|c| !c.is_empty()).map_or_else(|| name.to_string(), |c| format!("{c}::{name}"))
}

/// A name looked up on every LSP server in use: the matching symbols with the server that sent each,
/// and when none match, the nearest names the servers sent instead.
struct Found {
    asks: Vec<Ask>,
    hits: Vec<(usize, Value)>,
    nearest: Vec<(usize, Value)>,
    /// `rust-analyzer index warm`, per server.
    index: Vec<String>,
}

/// `workspace/symbol` for the name's last segment on each server that offers it, after the disk
/// sync. The matches are the symbols of that name (of `kinds`, if given) whose qualifiers fit, else
/// those whose name starts with it in any case; with none, the nearest are the fuzzy ones whose
/// qualifiers fit first, then the closest names (from the first 3 letters when the whole name
/// found nothing). An empty answer from an index that hasn't settled waits up to 20 s for it and
/// asks again. A server that fails is a note, unless every one does.
fn lookup(host: &Arc<Host>, caller: &BTreeMap<String, String>, verb: &str, name: &str, kinds: &[u64], cancel: &Cancel) -> Answer<Found> {
    let segs = segments(name);
    let Some((last, quals)) = segs.split_last() else { return Err(Box::new(Outcome::error(verb, "bad_args", "an empty name"))) };
    let mut found = Found { asks: servers(host, caller, verb, name, &|_| true, true, cancel)?.ready(verb, name)?.asks, hits: vec![], nearest: vec![], index: vec![] };
    let (mut errors, mut asked) = (Vec::new(), 0);
    for (i, ask) in found.asks.iter_mut().enumerate().filter(|(_, a)| a.offers("workspaceSymbolProvider")) {
        asked += 1;
        let settled = ask.catch_up(WAIT, cancel);
        let Ok(mut all) = ask.symbols(last, cancel).inspect_err(|e| errors.push((err_class(e), format!("{e:#}")))) else { continue };
        let mut hits = ask.matches(&all, last, quals, kinds, cancel);
        if hits.is_empty() && !settled {
            ask.notes.pop();
            ask.catch_up(COLD, cancel);
            all = ask.symbols(last, cancel).unwrap_or(all);
            hits = ask.matches(&all, last, quals, kinds, cancel);
        }
        found.index.push(ask.indexed());
        // Servers send fuzzy matches up to a limit of theirs; past many, the one named may be cut off.
        ask.notes
            .extend((hits.is_empty() && all.len() >= CUT).then(|| format!("{} sent {} fuzzy matches and none named {name}; its list may be cut off", ask.launch.id, all.len())));
        if hits.is_empty() && all.is_empty() && last.chars().count() > 3 {
            all = ask.symbols(&last.chars().take(3).collect::<String>(), cancel).unwrap_or_default();
        }
        let key = |s: &&Value| (!qualified(quals, &container(s), &ask.rel_of(s)), distance(&s["name"].as_str().unwrap_or("").to_lowercase(), &last.to_lowercase()));
        let mut near: Vec<&Value> = all.iter().filter(|_| hits.is_empty()).collect();
        near.sort_by_cached_key(key);
        found.nearest.extend(near.into_iter().take(5).map(|s| (i, s.clone())));
        found.hits.extend(hits.into_iter().map(|s| (i, s)));
    }
    if let Some((class, e)) = errors.first().filter(|_| errors.len() == asked) {
        return Err(Box::new(Outcome::error(verb, class, e.clone())));
    }
    if let Some(a) = found.asks.first_mut() {
        a.notes.extend(errors.into_iter().map(|e| e.1));
    }
    Ok(found)
}

impl Found {
    /// The matches as `S` items, or the nearest names when none match (EMPTY); an empty answer of a
    /// cold index says it may change.
    fn outcome(mut self, verb: &str, scope: &str) -> Outcome {
        let n = self.hits.len();
        let shown = if n == 0 { &self.nearest } else { &self.hits };
        let items: Vec<Item> = shown.iter().enumerate().map(|(k, (i, s))| self.asks[*i].item(sid(k + 1), kind(s), &s["location"], qualified_name(s))).collect();
        let nearest = (n == 0 && !items.is_empty()).then(|| format!("nearest: {}", items.iter().map(|i| format!("{} {}", i.id, i.msg)).collect::<Vec<_>>().join(", ")));
        let counts = [Some(format!("{n} match{}", if n == 1 { "" } else { "es" })), nearest, Some(self.index.join(", "))];
        let mut notes: Vec<String> = self.asks.iter_mut().flat_map(|a| std::mem::take(&mut a.notes)).collect();
        notes.extend((n == 0 && self.index.iter().any(|i| i.contains(" index cold"))).then(|| "index cold: an empty answer may change; retry".to_string()));
        let verdict = if n == 0 { Verdict::Empty } else { Verdict::Ok };
        Outcome { items, notes, ..Outcome::new(verb, verdict, scope, counts.into_iter().flatten().collect::<Vec<_>>().join("; ")) }
    }
}

/// `find <name> [--kind K]`: the symbols the name matches, each at its location with its source
/// line. The index flag says how much to trust an empty answer.
pub fn find(host: &Arc<Host>, caller: &BTreeMap<String, String>, name: &str, kind: Option<&str>, cancel: &Cancel) -> Outcome {
    let scope = kind.map_or_else(|| name.to_string(), |k| format!("{name} --kind {k}"));
    match kinds(kind).map_err(|e| Box::new(Outcome::error("find", "bad_args", e))).and_then(|k| lookup(host, caller, "find", name, &k, cancel)) {
        Ok(found) => found.outcome("find", &scope),
        Err(o) => Outcome { scope: if o.verdict == Verdict::Error { o.scope.clone() } else { scope }, ..*o },
    }
}

/// The server whose filetypes hold the extension of `file`, up for a query.
fn server_for(host: &Arc<Host>, caller: &BTreeMap<String, String>, verb: &str, scope: &str, file: &Path, cancel: &Cancel) -> Answer<Ask> {
    let ext = file.extension().and_then(OsStr::to_str).unwrap_or_default();
    let found = servers(host, caller, verb, scope, &|f| f.iter().any(|f| f == ext), true, cancel)?.ready(verb, scope)?;
    found.asks.into_iter().next().ok_or_else(|| Box::new(Outcome::new(verb, Verdict::Empty, scope, format!("no language server in use for .{ext} files"))))
}

/// What `refs` and `info` ask about: an `S` id from the newest result of the caller's session that
/// holds one, or from the result named (`q:4/` and the id), or a name that matches one symbol.
/// None is the answer instead (EMPTY); so are several, under an id of their own, with the
/// command that picks the first by its qualified id, which no later result can shadow.
/// Its server, with the symbol's file opened, the `textDocument` position, and the symbol as the
/// first `S` item.
fn target(host: &Arc<Host>, caller: &BTreeMap<String, String>, session: &str, verb: &str, name: &str, cancel: &Cancel) -> Answer<(Ask, Value, Item)> {
    let (q, sym) = name.split_once('/').filter(|(q, _)| q.starts_with("q:")).map_or((None, name), |(q, s)| (Some(q), s));
    let store = Store(host.ws.state_dir(&host.dirs));
    let (ask, item, path, line, col) = if sym.strip_prefix('S').is_some_and(|n| n.parse::<u32>().is_ok()) {
        let picked = q.map_or_else(|| store.holding(sym, session), |q| store.load(q).ok().map(|(o, _)| (q.to_string(), o)));
        let held = picked.and_then(|(id, o)| o.items.into_iter().find(|i| i.id == sym).and_then(|i| Some((id, i.loc.clone()?, i))));
        let Some((id, loc, item)) = held else { return Err(Box::new(Outcome::error(verb, "unknown_id", text("no_item", &[("id", &name)])))) };
        let mut ask = server_for(host, caller, verb, name, Path::new(&loc.path), cancel)?;
        ask.catch_up(WAIT, cancel);
        ask.notes.extend(q.is_none().then(|| text("picked", &[("item", &name), ("id", &id)])));
        // A location outside the workspace is shown under `~/`.
        let home = loc.path.strip_prefix("~/").and_then(|p| Some(PathBuf::from(std::env::var_os("HOME")?).join(p)));
        let path = home.unwrap_or_else(|| ask.root.join(&loc.path));
        let src = parse::source(&ask.root, &path.to_string_lossy(), loc.line).map(|s| s.1).unwrap_or_default();
        let col = server_col(&src, loc.col.saturating_sub(1) as usize, lock(&ask.slot.live).lsp.utf16);
        (ask, Item { id: sid(1), ..item }, path, loc.line.saturating_sub(1), col)
    } else {
        let found = lookup(host, caller, verb, name, &[], cancel)?;
        let n = found.hits.len();
        if n != 1 {
            let mut o = found.outcome(verb, name);
            if n > 1 {
                o.id = Next::take(&store.0, "q").ok();
                let next = Some(vec![verb.to_string(), o.id.as_ref().map_or_else(|| sid(1), |q| format!("{q}/{}", sid(1)))]);
                o.states.push(State { item: name.into(), state: "ambiguous".into(), reason: Some(format!("{n} matches: pick one by its id")), next, ..State::default() });
            }
            return Err(Box::new(o));
        }
        let Found { mut asks, hits, .. } = found;
        let (ask, s) = (asks.swap_remove(hits[0].0), &hits[0].1);
        let item = ask.item(sid(1), kind(s), &s["location"], qualified_name(s));
        let (start, path) = (&s["location"]["range"]["start"], path_of(s["location"]["uri"].as_str().unwrap_or("")).unwrap_or_default());
        (ask, item, path, start["line"].as_u64().unwrap_or(0) as u32, start["character"].as_u64().unwrap_or(0) as usize)
    };
    let uri = ask.open(&path).map_err(|e| failed(verb, e))?;
    Ok((ask, json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": col}}), item))
}

/// The outcome of a query, whichever way it ended.
fn answer(query: impl FnOnce() -> Answer<Outcome>) -> Outcome {
    query().unwrap_or_else(|o| *o)
}

/// `refs <name|S-id>`: the symbol, then its references (`includeDeclaration: false`) by file and line.
pub fn refs(host: &Arc<Host>, caller: &BTreeMap<String, String>, session: &str, name: &str, cancel: &Cancel) -> Outcome {
    answer(|| {
        let (mut ask, mut params, item) = target(host, caller, session, "refs", name, cancel)?;
        params["context"] = json!({"includeDeclaration": false});
        let sent = ask.request("textDocument/references", params, cancel).map_err(|e| failed("refs", e))?;
        let mut refs: Vec<Item> = sent.as_array().into_iter().flatten().map(|l| ask.item(String::new(), "ref", l, String::new())).collect();
        refs.sort_by_key(|r| r.loc.as_ref().map(|l| (l.path.clone(), l.line, l.col)));
        refs.iter_mut().enumerate().for_each(|(n, r)| r.id = format!("R{}", n + 1));
        let files = refs.iter().filter_map(|r| r.loc.as_ref().map(|l| &l.path)).collect::<BTreeSet<_>>().len();
        let counts = format!("{} in {}; {}", parse::plural(refs.len(), "reference"), parse::plural(files, "file"), ask.indexed());
        let verdict = if refs.is_empty() { Verdict::Empty } else { Verdict::Ok };
        let notes = std::mem::take(&mut ask.notes).into_iter().chain(["precise references only; strings and comments not searched".into()]).collect();
        Ok(Outcome { items: [item].into_iter().chain(refs).collect(), notes, ..Outcome::new("refs", verdict, name, counts) })
    })
}

/// `info <name|S-id>`: the symbol at its definition, with the hover's signature and docs as its
/// detail lines; markdown keeps its text, without fences and rules. Each is asked only of a server
/// that offers it, and a failed definition keeps the hover, with a note.
pub fn info(host: &Arc<Host>, caller: &BTreeMap<String, String>, session: &str, name: &str, cancel: &Cancel) -> Outcome {
    answer(|| {
        let (mut ask, params, mut item) = target(host, caller, session, "info", name, cancel)?;
        let hover = ask.offers("hoverProvider").then(|| ask.request("textDocument/hover", params.clone(), cancel)).transpose().map_err(|e| failed("info", e))?.unwrap_or_default();
        let def = ask.offers("definitionProvider").then(|| ask.request("textDocument/definition", params, cancel));
        ask.notes.extend(def.as_ref().and_then(|d| d.as_ref().err()).map(|e| format!("{e:#}")));
        // A Location, a list of them, or LocationLinks.
        let def = def.and_then(Result::ok).unwrap_or_default();
        let def = if def.is_array() { def[0].clone() } else { def };
        let def = if def["targetUri"].is_string() { json!({"uri": def["targetUri"], "range": def["targetSelectionRange"]}) } else { def };
        if def["uri"].as_str().and_then(path_of).is_some() {
            let at = ask.item(String::new(), "", &def, String::new());
            (item.loc, item.src) = (at.loc, at.src);
        }
        let contents = &hover["contents"];
        let parts: Vec<&str> = contents.as_array().map_or_else(|| vec![contents], |a| a.iter().collect()).into_iter().filter_map(|c| c.as_str().or(c["value"].as_str())).collect();
        let lines = parts.iter().flat_map(|p| p.lines()).map(str::trim_end).filter(|l| !l.is_empty() && !l.starts_with("```") && *l != "---");
        item.detail = lines.take(40).map(|l| clip(l, MAX_TEXT)).collect();
        let counts = format!("1 match; {}", ask.indexed());
        Ok(Outcome { items: vec![item], notes: std::mem::take(&mut ask.notes), ..Outcome::new("info", Verdict::Ok, name, counts) })
    })
}

/// `outline <path>`: the file's symbols from `textDocument/documentSymbol`, from the server whose
/// filetypes hold its extension; a nested one after its parent, two levels deep.
pub fn outline(host: &Arc<Host>, caller: &BTreeMap<String, String>, cwd: &Path, path: &str, cancel: &Cancel) -> Outcome {
    answer(|| {
        // From the caller's folder first, as every path the CLI takes, then from the root; one form of
        // each path, so `../a.rs` is not a second document.
        let file = [cwd.join(path), host.ws.root.join(path)].into_iter().find_map(|p| fs::canonicalize(p).ok().filter(|p| p.is_file()));
        let file = file.ok_or_else(|| Box::new(Outcome::error("outline", "bad_args", format!("no file {path}"))))?;
        let mut ask = server_for(host, caller, "outline", path, &file, cancel)?;
        let uri = ask.open(&file).map_err(|e| failed("outline", e))?;
        ask.catch_up(WAIT, cancel);
        let symbols = ask.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri}}), cancel).map_err(|e| failed("outline", e))?;
        // Hierarchical `DocumentSymbol`s, depth first; flat `SymbolInformation`s carry their location.
        let (mut todo, mut items): (Vec<(usize, &Value)>, Vec<Item>) = (symbols.as_array().into_iter().flatten().rev().map(|s| (0, s)).collect(), Vec::new());
        while let Some((depth, s)) = todo.pop() {
            let loc = if s["location"].is_object() { s["location"].clone() } else { json!({"uri": uri, "range": s["selectionRange"]}) };
            items.push(ask.item(sid(items.len() + 1), kind(s), &loc, format!("{}{}", "  ".repeat(depth), s["name"].as_str().unwrap_or(""))));
            todo.extend(s["children"].as_array().into_iter().flatten().rev().filter(|_| depth == 0).map(|c| (1, c)));
        }
        let counts = format!("{}; {}", parse::plural(items.len(), "symbol"), ask.indexed());
        Ok(Outcome { items, notes: std::mem::take(&mut ask.notes), ..Outcome::new("outline", Verdict::Ok, path, counts) })
    })
}

/// What the LSP servers in use pushed, for `problems`.
#[derive(Default)]
pub struct Pushed {
    pub items: Vec<Item>,
    /// Every server caught up before the deadline.
    pub settled: bool,
    /// `rust-analyzer index warm`, per server.
    pub index: Vec<String>,
    pub notes: Vec<String>,
    /// A server in scope may not start before trust: `rust-analyzer runs project code`, with the
    /// `untrusted` state row, which says who acts next.
    pub untrusted: Option<String>,
    pub states: Vec<State>,
    /// The servers in scope that weren't asked, because they weren't running on a call that starts
    /// none or aren't ready: their part of a `--new` baseline is kept as it was, not counted fixed.
    pub held: Vec<String>,
}

const SEVERITY: [&str; 4] = ["error", "warning", "info", "hint"];

/// The diagnostics of the servers whose filetypes the scope can hold (any, for a folder), for
/// `problems`: one not running starts unless `start` is false (a hook's call), and says its index is
/// cold; a file named is opened, since most servers push only for open documents; each is told
/// what changed on disk and has until `deadline` to settle. Pushes for files outside the workspace
/// are left out. A server that can't start, or may not before trust, makes the call unsettled,
/// with a note, so the actions' problems still show but the baseline stays; one not ready (a
/// missing package, a path install gone) is held, with a note, as one not running is: what it
/// pushed before stays in the baseline, and a server never installed doesn't stop it advancing.
pub fn problems(host: &Arc<Host>, caller: &BTreeMap<String, String>, scope: &str, start: bool, deadline: Duration, cancel: &Cancel) -> Pushed {
    let file = host.ws.root.join(scope);
    let ext = file.is_file().then(|| file.extension().and_then(OsStr::to_str).unwrap_or_default().to_string());
    let mut out = Pushed { settled: true, ..Pushed::default() };
    let found = match servers(host, caller, "problems", scope, &|f| ext.as_ref().is_none_or(|e| f.contains(e)), start, cancel) {
        Ok(found) => found,
        Err(o) if o.verdict == Verdict::Empty => return out,
        Err(o) if o.verdict == Verdict::Untrusted => return Pushed { untrusted: Some(o.counts), states: o.states, settled: false, ..out },
        Err(o) => return Pushed { notes: vec![format!("{}; its diagnostics are not included", o.counts)], settled: false, ..out },
    };
    if !found.untrusted.is_empty() {
        (out.untrusted, out.states, out.settled) = (Some(format!("{} runs project code", found.untrusted.join(", "))), found.states, false);
    }
    out.held = found.idle.iter().chain(&found.unready).cloned().collect();
    out.notes.extend(found.idle.iter().map(|id| format!("{id} not running: its diagnostics are not asked")));
    out.notes.extend((!found.unready.is_empty()).then(|| format!("{} not ready: its diagnostics are not included", found.unready.join(", "))));
    let end = Instant::now() + deadline;
    for mut ask in found.asks {
        // After the sync, which opens and saves it if it changed: a save is what starts a check.
        ask.sync();
        let opened = ext.as_ref().filter(|e| ask.launch.filetypes.contains(e)).map(|_| ask.open(&file));
        ask.notes.extend(opened.and_then(Result::err).map(|e| format!("{}: {e:#}", ask.launch.id)));
        if !ask.settle(end, cancel) {
            out.settled = false;
            let why = if ask.unheard() { "began no check after the last save, sent twice".to_string() } else { format!("not settled after {} s", deadline.as_secs_f64()) };
            ask.notes.push(format!("{} {why}: its diagnostics may be behind the disk", ask.launch.id));
        }
        out.index.push(ask.indexed());
        // Taken out under the lock first: making an item locks the slot again.
        let pushed: Vec<(String, Diag)> = {
            let live = lock(&ask.slot.live);
            let ours = live.lsp.diags.iter().filter(|(u, _)| live.epoch == ask.epoch && path_of(u).is_some_and(|p| p.starts_with(&ask.root)));
            ours.flat_map(|(u, d)| d.items.iter().map(move |x| (u.clone(), x.clone()))).collect()
        };
        for (uri, x) in pushed {
            let mut lines = x.message.lines().map(String::from);
            let first = lines.next().unwrap_or_default();
            let (loc, msg) = (json!({"uri": uri, "range": {"start": {"line": x.range[0], "character": x.range[1]}}}), x.code.map_or(first.clone(), |c| format!("{c}: {first}")));
            let kind = SEVERITY[x.severity.clamp(1, 4) as usize - 1];
            out.items.push(Item { detail: lines.collect(), sources: vec![ask.launch.id.clone()], ..ask.item(String::new(), kind, &loc, msg) });
        }
        out.notes.append(&mut ask.notes);
    }
    out
}

#[cfg(test)]
mod tests;
