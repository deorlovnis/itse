//! The store: packages fetched from sha256 pins into `~/.local/share/itse/store/`,
//! one read-only folder per pin, and the delegated tools that move and unpack their bytes.
//!
//! The core opens no network socket; every byte comes through curl and Halti's door. tar, gzip and
//! python3 unpack, keytool builds the JVM truststore. All of them sit behind `Delegate`, `Real` in
//! the daemon and a fake in tests (Halti's World). The core itself verifies the sha256, walks the
//! unpacked tree (no absolute links, nothing that leads out of it), makes it read-only and renames
//! it into place, so a package in the store is always whole.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Permissions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::lock::{self, Lock, Pin};
use crate::manifest::Package;
use crate::results::State;
use crate::sys;
use crate::workspace::Dirs;

/// How a delegated tool ended, and what it printed.
#[derive(Debug, Default)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The tools Itse moves and unpacks bytes with: curl, tar, gzip, python3 and keytool.
pub trait Delegate: Sync {
    /// Run `tool` (a name looked up on the caller's PATH, or a path) with `args`; its stdout goes
    /// to `stdout` when one is given, since a decompressed binary is no string.
    fn run(&self, tool: &str, args: &[String], stdout: Option<&Path>) -> Result<Out>;
}

/// The real tools, in the caller's env on top of the daemon's, never a cleared one: the door's
/// HTTPS_PROXY and CURL_CA_BUNDLE come with it. Debug builds look in `ITSE_TEST_DELEGATE_DIR` first.
pub struct Real(pub BTreeMap<String, String>);

impl Delegate for Real {
    fn run(&self, tool: &str, args: &[String], stdout: Option<&Path>) -> Result<Out> {
        let seam = self.0.get("ITSE_TEST_DELEGATE_DIR").filter(|_| cfg!(debug_assertions)).map(|d| Path::new(d).join(tool)).filter(|p| p.exists());
        let mut cmd = Command::new(seam.as_deref().unwrap_or(Path::new(tool)));
        cmd.args(args).envs(&self.0).stdin(Stdio::null());
        if let Some(path) = stdout {
            cmd.stdout(File::create(path).with_context(|| format!("creating {}", path.display()))?);
        }
        let out = cmd.output().with_context(|| format!("running {tool}"))?;
        Ok(Out { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into(), stderr: String::from_utf8_lossy(&out.stderr).into() })
    }
}

/// Why an install or a pin lookup failed, as a result row says it. A halti-owner's failure is the
/// door's (`blocked_by_door`), any other an `io` one.
#[derive(Debug)]
pub struct Fail {
    pub reason: String,
    /// `agent`, or `halti-owner` for what only the door's owner can change.
    pub actor: &'static str,
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        Self { reason: format!("{e:#}"), actor: "agent" }
    }
}

impl Fail {
    pub fn row(&self, item: &str) -> State {
        let state = if self.actor == "agent" { "failed" } else { "blocked" };
        State { item: item.into(), state: state.into(), reason: Some(self.reason.clone()), actor: Some(self.actor.into()), ..State::default() }
    }
}

/// `curl --fail-with-body -sSL --proto =https --proto-redir =https -w '%{http_connect} %{http_code}
/// %{url_effective}' [-C -] -o <out> <url>`: https only, redirects too (a pin source's JSON isn't
/// hashed); the codes and the last URL tell the door's refusals apart, and an HTTP error keeps its
/// body, since only the door's own answers start with `halti: `.
pub fn curl_args(url: &str, out: &Path, resume: bool) -> Vec<String> {
    let resume: &[&str] = if resume { &["-C", "-"] } else { &[] };
    let w = "%{http_connect} %{http_code} %{url_effective}";
    let head = ["--fail-with-body", "-sSL", "--proto", "=https", "--proto-redir", "=https", "-w", w].into_iter().chain(resume.iter().copied()).map(String::from);
    head.chain(["-o".into(), out.display().to_string(), url.into()]).collect()
}

/// `tar --no-same-owner -x -f <archive> -C <dir>`: GNU tar finds the compression itself.
pub fn tar_args(archive: &Path, dir: &Path) -> Vec<String> {
    ["--no-same-owner", "-x", "-f", &archive.display().to_string(), "-C", &dir.display().to_string()].map(String::from).to_vec()
}

/// `(http_connect, http_code, last url)` from curl's `-w` line; 0 for a code it didn't get to.
fn written(out: &Out) -> (u16, u16, Option<&str>) {
    let w: Vec<&str> = out.stdout.split_whitespace().collect();
    let code = |i: usize| w.get(i).and_then(|c| c.parse().ok()).unwrap_or(0);
    (code(0), code(1), w.get(2).copied())
}

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" | ")
}

/// What a failed curl means (as Halti's proxy.rs and inspect.rs answer): the door's
/// refusals name the host curl last asked (a redirect's, not the pin's) and the halti owner who
/// can open it. Inside the tunnel only an answer whose `body` starts `halti: ` is the door's; a
/// site's own 403 or 502 (a rate limit, an expired link) is the agent's, with curl's last words.
fn door_class(out: &Out, url: &str, body: &str) -> Fail {
    let (connect, code, last) = written(out);
    let host = last.unwrap_or(url).split_once("://").map_or(url, |u| u.1).split(['/', ':', '?']).next().unwrap_or_default();
    let door = match (connect, code, body.starts_with("halti: ")) {
        (403, _, _) => "blocked_by_door",
        (502, _, _) => "door_upstream_tls_failed",
        (_, 421, true) => "door_host_mismatch",
        (200, 403, true) => "door_refused_write",
        (200, 502, true) => "door_masked_encoding",
        _ => return Fail { reason: format!("delegate_failed curl exit {}: {}", out.code, last_lines(&out.stderr)), actor: "agent" },
    };
    Fail { reason: format!("{door} host {host}; actor halti-owner"), actor: "halti-owner" }
}

/// The body an HTTP error (curl exit 22) appended to `file` after its first `from` bytes, cut off
/// again so a resumed part stays whole. Any other failure keeps what arrived, for the next resume.
fn cut(out: &Out, file: &Path, from: u64) -> String {
    let mut body = Vec::new();
    if let Some(mut f) = File::options().read(true).write(true).open(file).ok().filter(|_| out.code == 22) {
        let _ = f.seek(SeekFrom::Start(from)).and_then(|_| (&mut f).take(4096).read_to_end(&mut body)).and_then(|_| f.set_len(from));
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Download `url` into `part`, resuming what an earlier try left. A server that ignores Range
/// (curl exit 33) or answers 416 gets the part deleted and one fresh try.
fn fetch(d: &dyn Delegate, url: &str, part: &Path, resume: bool) -> Result<(), Fail> {
    let before = fs::metadata(part).map_or(0, |m| m.len());
    let out = d.run("curl", &curl_args(url, part, resume), None)?;
    match out.code {
        0 => Ok(()),
        33 | 22 if resume && (out.code == 33 || written(&out).1 == 416) => {
            let _ = fs::remove_file(part);
            fetch(d, url, part, false)
        }
        _ => Err(door_class(&out, url, &cut(&out, part, before))),
    }
}

/// GET a JSON document (a pin source's metadata) through curl, with the door's classes on failure.
pub fn get_json(d: &dyn Delegate, dirs: &Dirs, url: &str) -> Result<Value, Fail> {
    let file = dirs.data.join("dl").join(format!("meta-{}.json", &sys::sha256_hex(url.as_bytes())[..16]));
    sys::private_dir(&dirs.data.join("dl"))?;
    let _ = fs::remove_file(&file);
    let out = d.run("curl", &curl_args(url, &file, false), None)?;
    if out.code != 0 {
        return Err(door_class(&out, url, &cut(&out, &file, 0)));
    }
    let value = fs::read(&file).context("reading the download").and_then(|b| serde_json::from_slice(&b).with_context(|| format!("{url} is not JSON")));
    let _ = fs::remove_file(&file);
    Ok(value?)
}

/// A file's sha256, streamed: a JDK is 200 MB.
pub fn file_sha256(path: &Path) -> Result<String> {
    let (mut file, mut hasher, mut buf) = (File::open(path).with_context(|| format!("reading {}", path.display()))?, Sha256::new(), vec![0; 1 << 20]);
    loop {
        match file.read(&mut buf)? {
            0 => return Ok(sys::hex(&hasher.finalize())),
            n => hasher.update(&buf[..n]),
        }
    }
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Install one pinned package and return its store folder: the store's copy if it
/// has one; else the download (resumed, or a complete part verified without curl) is checked
/// against the pin, unpacked into `sha256-<hex>.tmp`, walked, made read-only and renamed into place.
pub fn install(d: &dyn Delegate, dirs: &Dirs, pkg: &Package, pin: &Pin) -> Result<PathBuf, Fail> {
    let (store, dl, sha) = (dirs.data.join("store"), dirs.data.join("dl"), pin.sha256.as_str());
    if !is_sha256(sha) {
        return Err(anyhow!("the lock's sha256 {sha:?} is not 64 lowercase hex digits").into());
    }
    let dest = store.join(format!("sha256-{sha}"));
    if dest.is_dir() {
        return Ok(dest);
    }
    sys::private_dir(&dl)?;
    sys::private_dir(&store)?;
    // One installer per pin, across daemons: a second one waits, then finds the package in place.
    let guard = File::create(dl.join(format!("{sha}.lock"))).context("creating the install lock")?;
    guard.lock().context("locking the install")?;
    if dest.is_dir() {
        return Ok(dest);
    }
    let part = dl.join(format!("{sha}.part"));
    let complete = pin.size.is_some_and(|size| fs::metadata(&part).is_ok_and(|m| m.len() == size));
    if !complete || file_sha256(&part)? != sha {
        if complete {
            fs::remove_file(&part).context("removing a bad download")?;
        }
        fetch(d, &pin.url, &part, true)?;
        let got = file_sha256(&part)?;
        if got != sha {
            let _ = fs::remove_file(&part);
            return Err(Fail { reason: format!("sha256_mismatch: {} has {got}, the lock pins {sha}; the download is deleted", pin.url), actor: "agent" });
        }
    }
    let tmp = store.join(format!("sha256-{sha}.tmp"));
    remove(&tmp);
    fs::create_dir(&tmp).context("creating the unpack folder")?;
    let unpacked = extract(d, pkg, &part, &tmp).and_then(|root| {
        let canon = fs::canonicalize(&root)?;
        seal(&canon, &canon)?;
        fs::rename(&root, &dest).context("moving the package into the store")?;
        Ok(fs::set_permissions(&dest, Permissions::from_mode(0o555))?)
    });
    remove(&tmp);
    unpacked.map_err(Fail::from)?;
    let _ = fs::remove_file(&part);
    Ok(dest)
}

/// Unpack with the delegated tool into `tmp`, mark each `bin` executable, and return the package
/// root `strip` folders down. `.zst` and other unknown kinds are refused.
fn extract(d: &dyn Delegate, pkg: &Package, part: &Path, tmp: &Path) -> Result<PathBuf> {
    // Each bin is a plain relative path, checked before a gz or a bare binary is written by its name.
    if let Some(rel) = pkg.bin.values().find(|rel| !Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))) {
        bail!("bin {rel} is not a file in the package");
    }
    let single = || -> Result<PathBuf> {
        let file = tmp.join(pkg.bin.values().next().map_or("bin", String::as_str));
        fs::create_dir_all(file.parent().unwrap_or(tmp))?;
        Ok(file)
    };
    let (part_s, tmp_s) = (part.display().to_string(), tmp.display().to_string());
    let (tool, args, stdout) = match pkg.archive.as_deref() {
        Some("tar.gz" | "tgz" | "tar.xz" | "tar") => ("tar", tar_args(part, tmp), None),
        Some("gz") => ("gzip", vec!["-dc".into(), part_s], Some(single()?)),
        Some("zip" | "vsix") => ("python3", ["-I", "-m", "zipfile", "-e", &part_s, &tmp_s].map(String::from).to_vec(), None),
        Some(other) => bail!("archive kind {other} is refused; tar.gz, tar.xz, gz, zip and vsix are known"),
        None => {
            fs::copy(part, single()?)?;
            ("", vec![], None)
        }
    };
    if !tool.is_empty() {
        let out = d.run(tool, &args, stdout.as_deref())?;
        if out.code != 0 {
            bail!("delegate_failed {tool} exit {}: {}", out.code, last_lines(&out.stderr));
        }
    }
    let mut root = tmp.to_path_buf();
    for _ in 0..pkg.strip {
        let subs: Vec<PathBuf> = fs::read_dir(&root)?.flatten().map(|e| e.path()).collect();
        match &subs[..] {
            [one] if fs::symlink_metadata(one).is_ok_and(|m| m.is_dir()) => root = one.clone(),
            _ => bail!("strip {} needs one top folder; the archive has {} entries", pkg.strip, subs.len()),
        }
    }
    for rel in pkg.bin.values() {
        let path = root.join(rel);
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            bail!("bin {rel} is not a file in the package");
        }
        fs::set_permissions(&path, Permissions::from_mode(0o755))?;
    }
    Ok(root)
}

/// Walk `dir` and make everything in it read-only. Absolute symlinks and links that lead out of
/// `canon` (lexically, or through a chain that exists) are refused; so are devices and fifos.
fn seal(canon: &Path, dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let (meta, rel) = (fs::symlink_metadata(&path)?, path.strip_prefix(canon).unwrap_or(&path).to_path_buf());
        if meta.is_symlink() {
            let to = fs::read_link(&path)?;
            let mut depth = rel.components().count() as i64 - 1;
            let out = to.components().any(|c| {
                depth += match c {
                    Component::ParentDir => -1,
                    Component::Normal(_) => 1,
                    _ => 0,
                };
                depth < 0
            });
            if to.is_absolute() || out || fs::canonicalize(&path).is_ok_and(|p| !p.starts_with(canon)) {
                bail!("{} links to {}, outside the package; refused", rel.display(), to.display());
            }
            continue;
        }
        if meta.is_dir() {
            seal(canon, &path)?;
        } else if !meta.is_file() {
            bail!("{} is not a file, folder or link; refused", rel.display());
        }
        fs::set_permissions(&path, Permissions::from_mode(meta.permissions().mode() & 0o555))?;
    }
    Ok(())
}

/// Remove a folder tree the store made read-only: u+w back on its folders first (never through
/// links), or nothing in them could be unlinked.
fn remove(path: &Path) {
    fn writable(path: &Path) {
        if let Some(meta) = fs::symlink_metadata(path).ok().filter(|m| m.is_dir()) {
            let _ = fs::set_permissions(path, Permissions::from_mode(meta.permissions().mode() | 0o700));
            fs::read_dir(path).into_iter().flatten().flatten().for_each(|e| writable(&e.path()));
        }
    }
    writable(path);
    let _ = if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
}

fn entries(dir: &Path) -> impl Iterator<Item = (String, PathBuf)> {
    fs::read_dir(dir).into_iter().flatten().flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
}

/// At daemon start: `.tmp` folders an install left when it died are removed, unless an install of
/// that pin (in another workspace's daemon) still holds its lock.
pub fn clean(dirs: &Dirs) {
    for (name, path) in entries(&dirs.data.join("store")).filter(|(n, _)| n.ends_with(".tmp")) {
        let sha = name.trim_start_matches("sha256-").trim_end_matches(".tmp");
        if File::open(dirs.data.join("dl").join(format!("{sha}.lock"))).is_ok_and(|f| f.try_lock().is_err()) {
            continue;
        }
        remove(&path);
    }
}

/// `gc --keep-locked`: remove the packages and downloads that no lock pins: every repo's machine
/// lock, and the team lock of every workspace that ever had a daemon. Returns what went.
pub fn gc(dirs: &Dirs) -> Result<Vec<String>> {
    let mut locks: Vec<PathBuf> = entries(&dirs.state.join("repo")).map(|(_, p)| p.join("itse.lock.json")).collect();
    locks.extend(entries(&dirs.state.join("ws")).filter_map(|(_, p)| fs::read_to_string(p.join("root")).ok()).map(|r| Path::new(&r).join(".itse/itse.lock.json")));
    // A lock that can't be read may pin anything, so gc fails closed instead of guessing.
    let read: Vec<Lock> = locks.iter().map(|p| lock::read(p)).collect::<Result<_>>().context("gc removes nothing while a lock can't be read")?;
    let pinned: BTreeSet<String> = read.into_iter().flat_map(|l| l.packages.into_values()).flat_map(|p| p.targets.into_values()).map(|t| t.sha256).collect();
    let mut gone = Vec::new();
    for (name, path) in entries(&dirs.data.join("store")).chain(entries(&dirs.data.join("dl"))) {
        let sha = name.trim_start_matches("sha256-").split('.').next().unwrap_or_default();
        // An install lock goes with its pin, unless an install holds it now.
        let held = name.ends_with(".lock") && File::open(&path).is_ok_and(|f| f.try_lock().is_err());
        if !name.ends_with(".tmp") && !held && !pinned.contains(sha) {
            remove(&path);
            gone.push(name);
        }
    }
    Ok(gone)
}

/// The resolver's row of package `name` (its item is `name` or `name <version>`).
pub fn row_of<'a>(states: &'a [State], name: &str) -> Option<&'a State> {
    states.iter().find(|s| s.item == name || s.item.strip_prefix(name).is_some_and(|v| v.starts_with(' ') && !v.ends_with(')')))
}

#[cfg(test)]
pub mod tests;
