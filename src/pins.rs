//! Pin sources (design §4.5): `itse lock --refresh` asks each package's upstream for its newest
//! build and the sha256 the upstream itself publishes: a GitHub release asset's `digest`, or the
//! `server-bundle.json` of an Open VSX extension. Metadata comes through curl and the door like
//! any byte. A refresh writes the repo's lock and says what changed; it installs nothing, and it
//! never touches trust (§10), so an agent can refresh an expiring pin mid-task.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::lock::{self, Lock, LockedPackage, Pin};
use crate::manifest::PinSource;
use crate::resolve::Resolved;
use crate::results::State;
use crate::store::{Delegate, Fail, get_json, is_sha256};
use crate::workspace::{Dirs, Workspace};

/// `*` matches any run of characters, as in the jdk's `OpenJDK25U-jdk_aarch64_linux_hotspot_*.tar.gz`.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => name.starts_with(head) && (head.len()..=name.len()).any(|i| name.is_char_boundary(i) && glob(tail, &name[i..])),
    }
}

/// The newest build for `target`: `(version, published date, pin)`. What upstream sends is checked
/// before it reaches the lock: an https URL and a 64-hex sha256, or a failed row now rather than a
/// failed install later.
pub fn lookup(d: &dyn Delegate, dirs: &Dirs, src: &PinSource, target: &str) -> Result<(String, String, Pin), Fail> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let none = || anyhow!("the pin source names no build for {target}");
    let (version, published, pin) = match src {
        PinSource::GithubRelease { repo, assets } => {
            let rel = get_json(d, dirs, &format!("https://api.github.com/repos/{repo}/releases/latest"))?;
            let pattern = assets.get(target).ok_or_else(none)?;
            let asset = rel["assets"].as_array().into_iter().flatten().find(|a| glob(pattern, &s(&a["name"]))).ok_or_else(|| anyhow!("{repo} has no asset {pattern}"))?;
            let sha = s(&asset["digest"]).trim_start_matches("sha256:").to_string();
            let pin = Pin { url: s(&asset["browser_download_url"]), sha256: sha, size: asset["size"].as_u64(), provenance: "upstream:github-digest".into() };
            (s(&rel["tag_name"]), s(&rel["published_at"]), pin)
        }
        // The bundle names the server archive and its sha256; `files.sha256` hashes the .vsix, which is never fetched.
        PinSource::OpenvsxBundle { extension, platforms } => {
            let platform = platforms.get(target).ok_or_else(none)?;
            let meta = get_json(d, dirs, &format!("https://open-vsx.org/api/{extension}/{platform}/latest"))?;
            let url = format!("https://open-vsx.org/vscode/unpkg/{extension}/{}+{platform}/extension/server-bundle.json", s(&meta["version"]));
            let b = get_json(d, dirs, &url)?;
            (s(&b["version"]), s(&meta["timestamp"]), Pin { url: s(&b["url"]), sha256: s(&b["sha256"]), size: None, provenance: "upstream:openvsx-bundle".into() })
        }
    };
    if !is_sha256(&pin.sha256) || !pin.url.starts_with("https://") || version.is_empty() {
        return Err(anyhow!("the pin source sent no https url, sha256 or version for {target}: url {:?}, sha256 {:?}", pin.url, pin.sha256).into());
    }
    Ok((version, published, pin))
}

/// `YYYY-MM-DD` plus `days`, through days since 1970 (days-from-civil, no time crate).
fn add_days(date: &str, days: u32) -> Option<String> {
    let mut p = date.get(..10)?.split('-').map(|n| n.parse::<i64>().ok());
    let (y, m, d) = (p.next()??, p.next()??, p.next()??);
    let y = if m <= 2 { y - 1 } else { y };
    let doe = y.rem_euclid(400) * 365 + y.rem_euclid(400) / 4 - y.rem_euclid(400) / 100 + (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let since = y.div_euclid(400) * 146_097 + doe - 719_468 + i64::from(days);
    Some(crate::sys::utc(UNIX_EPOCH + Duration::from_secs(u64::try_from(since * 86_400).ok()?))[..10].to_string())
}

/// `itse lock [--refresh] [pkg] [--target T]`: pins for the packages in use that have a pin
/// source, written to this repo's lock; one row per package saying what changed, and the lock's
/// path. Without `--refresh` only missing pins are looked up.
pub fn refresh(d: &dyn Delegate, dirs: &Dirs, ws: &Workspace, r: &Resolved, only: Option<&str>, target: &str, all: bool) -> Result<(Vec<State>, PathBuf)> {
    let path = lock::repo_dir(dirs, ws).join("itse.lock.json");
    let (mut lock, mut rows): (Lock, Vec<State>) = (lock::read(&path)?, Vec::new());
    for (ext, name, pkg) in r.manifest.uses().iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| e.0.packages.iter().map(move |p| (&e.0, p.0, p.1))) {
        let old = lock.packages.get(name).and_then(|l| Some((l.purl.clone(), l.targets.get(target)?.sha256.clone())));
        let Some(src) = pkg.pin_source.as_ref().filter(|_| only.is_none_or(|o| o == name) && (all || old.is_none())) else { continue };
        let Ok((version, published, pin)) = lookup(d, dirs, src, target).map_err(|f| rows.push(State { hosts: pkg.hosts.clone(), ..f.row(name) })) else { continue };
        let ((was, sha), purl, expires) = (old.unwrap_or_default(), format!("{}@{version}", pkg.purl), pkg.expires_after_days.and_then(|n| add_days(&published, n)));
        let (state, next) = if sha == pin.sha256 { ("unchanged", None) } else { ("changed", Some(vec!["install".to_string(), name.clone()])) };
        let ends = expires.as_ref().map_or(String::new(), |e| format!("; expires about {e}"));
        let reason = format!("{} → {version}, sha256 {}…{ends}", was.rsplit_once('@').map_or("none", |v| v.1), pin.sha256.get(..12).unwrap_or(&pin.sha256));
        rows.push(State { item: name.clone(), state: state.into(), reason: Some(reason), next, ..State::default() });
        let entry = lock.packages.entry(name.clone()).or_insert_with(LockedPackage::default);
        entry.targets.retain(|_, _| entry.purl == purl);
        let extension = format!("{}@{}", ext.id, ext.version.as_deref().unwrap_or("0"));
        (entry.purl, entry.install, entry.published, entry.expires_estimate, entry.extension) = (purl, "fetch".into(), published.get(..10).map(String::from), expires, extension);
        entry.targets.insert(target.into(), pin);
    }
    lock::write(&path, &Lock { itse_lock: 1, ..lock })?;
    Ok((rows, path))
}

#[cfg(test)]
mod tests;
