//! Per-repo and per-machine records of decisions: the lock with its pins, trust, consents, found
//! tools and added extensions (design §3.2, §4.1, §10). Every record lives in machine state, never
//! in the repo; a team lock in `.itse/` has the lock's shape and is only read.
//!
//! Trust, consent and `ext add` are human-only. In I1 they are advisory (decision 6): MCP gets
//! `NEEDS_HUMAN`, and the CLI runs them and records whether a terminal was attached. Consent alone
//! refuses a caller without one (decision 14).

// Pin fields that the store and `lock --refresh` read arrive with them.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, os::unix::fs::MetadataExt};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::ConsentSpec;
use crate::results::{Outcome, State, Verdict, text};
use crate::sys;
use crate::workspace::{Dirs, Workspace};

/// `itse.lock.json`: fetched packages only, keyed by target.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lock {
    pub itse_lock: u32,
    pub packages: BTreeMap<String, LockedPackage>,
    /// What the repo pins itself, such as `gradle-wrapper: {distributionSha256Sum: "present"}`.
    pub project_pins: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LockedPackage {
    pub purl: String,
    pub install: String,
    pub targets: BTreeMap<String, Pin>,
    pub published: Option<String>,
    pub expires_estimate: Option<String>,
    /// `<extension id>@<version>` that declared the package.
    pub extension: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pin {
    pub url: String,
    pub sha256: String,
    pub size: Option<u64>,
    /// `upstream:github-digest`, `upstream:openvsx-bundle`, `upstream:sha256-file`, `image` or `tofu`.
    pub provenance: String,
}

/// `repo/<rid>/trust.json`: the last 8 accepted trust hashes, so switching branches doesn't ask each time.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Trust {
    pub repo: String,
    pub accepted: Vec<String>,
    /// `tty` or `non-tty`: advisory in I1, recorded for the human to see.
    pub actor: String,
    pub at: String,
}

/// One entry of `consents.json`, keyed by package. A new EULA hash asks again.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Consent {
    pub eula_hash: Option<String>,
    pub region: Option<String>,
    pub data_sharing: Option<String>,
    pub actor: String,
    pub at: String,
}

/// One entry of `found.json`, keyed by package: a `path` install as found.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Found {
    pub path: String,
    pub version: Option<String>,
    pub sha256: String,
    /// Size and mtime when hashed: the binary is hashed again only when they change.
    pub size: u64,
    pub mtime: i64,
}

/// One entry of `extensions.json`, keyed by id: an extension added with `itse ext add`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Added {
    pub sha256: String,
    pub actor: String,
    pub at: String,
}

/// A record, or its default when the file doesn't exist; a broken one is an error that names it.
pub fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A record as pretty JSON, 0600 in a 0700 dir, replaced atomically.
pub fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    write_bytes(path, &[serde_json::to_vec_pretty(value)?, b"\n".to_vec()].concat())
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    sys::private_dir(path.parent().unwrap_or(path))?;
    sys::write_atomic(path, bytes)
}

/// `state/repo/<rid>/`: the lock and trust of a repo, shared by its worktrees.
pub fn repo_dir(dirs: &Dirs, ws: &Workspace) -> PathBuf {
    dirs.state.join("repo").join(&ws.rid)
}

impl Lock {
    /// This repo's machine lock. A team lock in `.itse/` seeds the packages it doesn't have yet,
    /// but only once the workspace is trusted: before that the repo's pins would choose what runs,
    /// so they are only named (the second value), for status.
    pub fn load(dirs: &Dirs, ws: &Workspace, trusted: bool) -> Result<(Lock, Vec<String>)> {
        let (mut lock, mut waiting): (Lock, _) = (read(&repo_dir(dirs, ws).join("itse.lock.json"))?, Vec::new());
        for (name, pkg) in read::<Lock>(&ws.root.join(".itse/itse.lock.json"))?.packages {
            if lock.packages.contains_key(&name) {
                continue;
            }
            match trusted {
                true => drop(lock.packages.insert(name, pkg)),
                false => waiting.push(name),
            }
        }
        Ok((lock, waiting))
    }
}

/// The trust hash covers only what trust is about: the team-layer files' bytes, then the
/// unrendered launch fields of each entry in use that runs project code. Pins, store paths,
/// versions and settings stay out, so a refresh, an install or an upgrade keeps trust.
pub fn trust_hash(team: &[u8], entries: &[Value]) -> String {
    sys::sha256_hex(&[team, serde_json::to_string(entries).unwrap_or_default().as_bytes()].concat())
}

pub fn trusted(dirs: &Dirs, ws: &Workspace, hash: &str) -> Result<bool> {
    Ok(read::<Trust>(&repo_dir(dirs, ws).join("trust.json"))?.accepted.iter().any(|h| h == hash))
}

/// `itse trust [--revoke]`: accept the current hash, keeping the last 8 accepted so switching
/// branches doesn't ask each time, or forget them all.
pub fn trust(dirs: &Dirs, ws: &Workspace, hash: &str, revoke: bool, actor: &str) -> Result<Outcome> {
    let path = repo_dir(dirs, ws).join("trust.json");
    let mut t: Trust = read(&path)?;
    t.accepted.retain(|h| h != hash && !revoke);
    if !revoke {
        t.accepted.push(hash.into());
        t.accepted.drain(..t.accepted.len().saturating_sub(8));
    }
    (t.repo, t.actor, t.at) = (ws.repo.display().to_string(), actor.into(), sys::utc(SystemTime::now()));
    write(&path, &t)?;
    let (state, reason) = if revoke { ("untrusted", "trust revoked") } else { ("ready", "trusted") };
    let row = State { item: "workspace".into(), state: state.into(), reason: Some(format!("{reason} by {actor}; hash {}", &hash[..12])), ..State::default() };
    Ok(Outcome { states: vec![row], ..Outcome::new("trust", Verdict::Ok, ws.root.display().to_string(), reason) })
}

pub fn consents(dirs: &Dirs) -> Result<BTreeMap<String, Consent>> {
    read(&dirs.state.join("consents.json"))
}

/// What is recorded for one declared consent: the EULA hash for a `hash` consent, else the choice.
pub fn consented<'a>(c: &'a Consent, spec: &ConsentSpec) -> Option<&'a str> {
    match spec.id.as_str() {
        _ if spec.kind == "hash" => c.eula_hash.as_deref(),
        "region" => c.region.as_deref(),
        "data_sharing" => c.data_sharing.as_deref(),
        _ => None,
    }
}

/// The hash a `hash` consent is given to: sha256 of `of_file` in the installed package, cut to
/// `hex_prefix` digits. A new file asks again.
pub fn file_hash(root: &Path, spec: &ConsentSpec) -> Option<String> {
    let hex = sys::sha256_hex(&fs::read(root.join(spec.of_file.as_deref()?)).ok()?);
    Some(hex[..spec.hex_prefix.unwrap_or(64).min(64)].into())
}

/// The ids of declared consents not recorded for `pkg`, the EULA counted only for its current hash.
pub fn missing_consents(all: &BTreeMap<String, Consent>, pkg: &str, specs: &[ConsentSpec], root: Option<&Path>) -> Vec<String> {
    let given = |s: &ConsentSpec| all.get(pkg).and_then(|c| consented(c, s)).map(String::from);
    specs.iter().filter(|s| given(s).is_none() || (s.kind == "hash" && given(s) != root.and_then(|r| file_hash(r, s)))).map(|s| s.id.clone()).collect()
}

/// `itse consent <pkg> [--eula-hash H] [--region R] [--data-sharing V]`: record what a human agreed
/// to. Nothing is defaulted, and a caller without a terminal is refused (decision 14).
pub fn consent(dirs: &Dirs, pkg: &str, specs: &[ConsentSpec], root: Option<&Path>, flags: &BTreeMap<String, Value>, tty: bool) -> Result<Outcome> {
    if !tty {
        let next = crate::mcp::next(&["consent".into(), pkg.into()], "cli", true);
        return Ok(Outcome { err: Some("needs_human"), ..Outcome::new("consent", Verdict::NeedsHuman, pkg, text("human", &[("cmd", &next)])) });
    }
    if specs.is_empty() {
        bail!("{pkg} declares no consents");
    }
    let flag = |k: &str| flags.get(k).and_then(Value::as_str).map(String::from);
    let mut c = Consent { eula_hash: flag("eula-hash"), region: flag("region"), data_sharing: flag("data-sharing"), actor: "tty".into(), at: sys::utc(SystemTime::now()) };
    let mut needs = Vec::new();
    for s in specs {
        let given = consented(&c, s).map(String::from);
        if s.kind == "hash" {
            let file = root.zip(s.of_file.as_deref()).map(|(r, f)| r.join(f));
            let (Some(file), Some(hash)) = (file, root.and_then(|r| file_hash(r, s))) else { bail!("{pkg} is not installed; its {} is read from the package", s.id) };
            if given.as_ref() != Some(&hash) {
                needs.push(format!("--eula-hash {hash} accepts {}", file.display()));
            }
        } else if given.is_none() || !(s.values.is_empty() || s.values.iter().any(|v| Some(v) == given.as_ref())) {
            needs.push(format!("--{} {}", s.id.replace('_', "-"), if s.values.is_empty() { "V".into() } else { s.values.join("|") }));
        }
    }
    if !needs.is_empty() {
        return Ok(Outcome { err: Some("needs_consent"), notes: needs, ..Outcome::new("consent", Verdict::NeedsConsent, pkg, "nothing recorded") });
    }
    let mut all = consents(dirs)?;
    c.eula_hash = c.eula_hash.filter(|_| specs.iter().any(|s| s.kind == "hash"));
    all.insert(pkg.into(), c);
    write(&dirs.state.join("consents.json"), &all)?;
    let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    Ok(Outcome::new("consent", Verdict::Ok, pkg, format!("recorded {} by tty", ids.join(", "))))
}

/// A `path` install as found, in `found.json`: the binary is hashed again only when its path,
/// size or mtime changed.
pub fn found(dirs: &Dirs, pkg: &str, path: &Path) -> Result<Found> {
    let file = dirs.state.join("found.json");
    let (mut all, meta) = (read::<BTreeMap<String, Found>>(&file)?, fs::metadata(path)?);
    let same = |f: &Found| f.path == path.to_string_lossy() && f.size == meta.size() && f.mtime == meta.mtime();
    if !all.get(pkg).is_some_and(same) {
        let sha256 = sys::sha256_hex(&fs::read(path)?);
        all.insert(pkg.into(), Found { path: path.to_string_lossy().into(), version: None, sha256, size: meta.size(), mtime: meta.mtime() });
        write(&file, &all)?;
    }
    all.remove(pkg).context("found.json")
}

pub fn added(dirs: &Dirs) -> Result<BTreeMap<String, Added>> {
    read(&dirs.state.join("extensions.json"))
}

/// Record an added extension's sha256 and actor; returns the hash.
pub fn record_added(dirs: &Dirs, id: &str, bytes: &[u8], actor: &str) -> Result<String> {
    let (mut all, sha256) = (added(dirs)?, sys::sha256_hex(bytes));
    all.insert(id.into(), Added { sha256: sha256.clone(), actor: actor.into(), at: sys::utc(SystemTime::now()) });
    write(&dirs.state.join("extensions.json"), &all)?;
    Ok(sha256)
}

/// Forget an added extension; false if none was recorded.
pub fn record_removed(dirs: &Dirs, id: &str) -> Result<bool> {
    let mut all = added(dirs)?;
    let had = all.remove(id).is_some();
    write(&dirs.state.join("extensions.json"), &all)?;
    Ok(had)
}

#[cfg(test)]
mod tests;
