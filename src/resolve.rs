//! The resolver: it merges the layers, runs detection, reads the lock, the store and
//! the project's own pins, expands `requires`, renders templates, and gives every item one state
//! with its actor and next step. Only `ready` items (and `unpinned` and `expiring` ones, which
//! still run) become a `Launch`; every other state is the answer, never an empty one.
//!
//! Resolving reads files. The one command it may run is a layer's typed `system` check (an argv,
//! never a shell string), and a team layer's only after `itse trust`. Nothing that runs project
//! code is launched while the workspace is untrusted.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use std::{env, fs};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::host::{Launch, Protocol};
use crate::lock::{self, Consent, Lock};
use crate::manifest::{DapVia, Manifest, Package, SystemCheck, Use, render, render_value};
use crate::results::{Outcome, State};
use crate::sys;
use crate::verbs::Args;
use crate::workspace::{Dirs, Workspace};

pub struct Resolved {
    pub manifest: Manifest,
    pub states: Vec<State>,
    /// What the session host may start.
    pub launches: Vec<Launch>,
    pub notes: Vec<String>,
    pub trust_hash: String,
    pub trusted: bool,
    /// The servers left out of `launches` until trust: those that (or whose packages) run project
    /// code, and every one from the team layer.
    pub held: Vec<String>,
    /// Each present package's root: its store dir, or the folder a `path` install was found in.
    pub roots: BTreeMap<String, PathBuf>,
}

/// The lock target of this machine: `uname -m` as the pin sources name it.
pub fn target() -> &'static str {
    match env::consts::ARCH {
        "aarch64" => "linux_arm64",
        "x86_64" => "linux_x64",
        other => other,
    }
}

fn row(item: impl Into<String>, state: &str, reason: impl Into<String>, actor: Option<&str>, next: &[&str]) -> State {
    let next = (!next.is_empty()).then(|| next.iter().map(|s| s.to_string()).collect());
    State { item: item.into(), state: state.into(), reason: Some(reason.into()), actor: actor.map(String::from), next, ..State::default() }
}

/// What resolving one package needs besides the package.
struct Cx<'a> {
    ws: &'a Workspace,
    dirs: &'a Dirs,
    path: Option<&'a String>,
    lock: Lock,
    /// Packages only the team lock pins, while the workspace is untrusted.
    waiting: Vec<String>,
    consents: BTreeMap<String, Consent>,
    today: String,
    soon: String,
}

/// Resolve the workspace for a caller whose env is `env` (its PATH finds `path` installs).
pub fn resolve(ws: &Workspace, dirs: &Dirs, env: &BTreeMap<String, String>, now: SystemTime) -> Result<Resolved> {
    resolve_for(ws, dirs, env, now, true)
}

/// `resolve`, with the typed system checks only when `checks`: they gate status and server starts,
/// and a shim, `exec` or `env` must not wait seconds on `docker info` before each tool call.
pub fn resolve_for(ws: &Workspace, dirs: &Dirs, env: &BTreeMap<String, String>, now: SystemTime, checks: bool) -> Result<Resolved> {
    let m = Manifest::load(ws, dirs)?;
    let uses = m.uses();
    let mut code = Vec::new();
    for ext in uses.iter().filter_map(|u| m.exts.get(&u.id)).map(|e| &e.0) {
        // A debug adapter runs the project's code: it launches the tests (as `via` and the debug
        // block say), or evaluates in them.
        for s in ext.servers.iter().filter(|s| s.executes_project_code || s.protocol == Protocol::Dap || m.packages_of(s, &uses).any(|p| p.executes_project_code)) {
            let mut c = json!({"server": s.id, "command": s.command, "env": s.env});
            // What runs only: the stop rule's fields don't.
            s.via.as_ref().inspect(|via| c["via"] = json!(via));
            ext.debug.as_ref().filter(|d| d.adapter == s.id).inspect(|d| c["debug"] = json!({"request": d.request, "arguments": d.arguments, "attach": d.attach}));
            code.push((s.id.clone(), c));
        }
        for a in ext.actions.iter().filter(|a| a.executes_project_code) {
            code.push((a.label.clone(), json!({"action": a.label, "command": a.command, "args": a.args, "cwd": a.options.cwd, "env": a.options.env})));
        }
    }
    // A team lock is a team-layer file too: its pins would choose what runs, so it asks for trust.
    let (team, trust_hash) = (!m.team_bytes.is_empty(), lock::trust_hash(&m.team_bytes, &code.iter().map(|c| c.1.clone()).collect::<Vec<_>>()));
    let needs_trust = team || !code.is_empty();
    let trusted = !needs_trust || lock::trusted(dirs, ws, &trust_hash)?;
    let (lock, waiting) = Lock::load(dirs, ws, trusted)?;
    let day = |d: u64| sys::utc(now + Duration::from_secs(d * 86_400))[..10].to_string();
    let cx = Cx { ws, dirs, path: env.get("PATH"), lock, waiting, consents: lock::consents(dirs)?, today: day(0), soon: day(7) };
    let (mut states, mut roots, mut ok) = (Vec::new(), BTreeMap::new(), BTreeSet::new());
    for u in &uses {
        let Some((ext, _)) = m.exts.get(&u.id) else {
            states.push(row(format!("{} (extension)", u.id), "missing", "no such extension", Some("agent"), &["ext", "list"]));
            continue;
        };
        for (name, pkg) in &ext.packages {
            // install=fetch can only move a package that has a pin source; the others keep theirs.
            let install = u.settings.get("install").and_then(Value::as_str).filter(|i| *i == "path" || pkg.pin_source.is_some()).unwrap_or(&pkg.install);
            let (state, root) = package(&cx, &u.id, name, pkg, install)?;
            if let Some(root) = root {
                if matches!(state.state.as_str(), "ready" | "unpinned" | "expiring") {
                    ok.insert(name.clone());
                }
                roots.insert(name.clone(), root);
            }
            states.push(state);
        }
    }
    let names: Vec<&str> = team.then_some("the team layer .itse/").into_iter().chain(code.iter().map(|c| c.0.as_str())).collect();
    match (needs_trust, trusted) {
        (true, false) => states.push(row("workspace", "untrusted", format!("project code runs in {}", names.join(", ")), Some("human"), &["trust"])),
        (true, true) => states.push(row("workspace", "ready", "trusted", None, &[])),
        _ => {}
    }
    for p in uses.iter().filter_map(|u| m.exts.get(&u.id)).flat_map(|e| &e.0.project_pins) {
        let item = format!("{} (project)", p.name);
        if let Some(values) = p.read(&ws.root).filter(|_| !states.iter().any(|s| s.item == item)) {
            states.push(row(item, "ready", format!("{}: {values}", p.file), None, &[]));
        }
    }
    for (name, (check, team)) in m.merged(|l| &l.system).into_iter().filter(|_| checks) {
        let item = format!("{name} (system)");
        // The trust check comes first: a team layer's command must not even start before trust.
        if team && !trusted && !check.cmd.is_empty() {
            states.push(row(item, "untrusted", "a team-layer check runs a command", Some("human"), &["trust"]));
            continue;
        }
        states.push(match system_check(check) {
            Ok(()) => row(item, "ready", check.lib.clone().unwrap_or_else(|| check.cmd.join(" ")), None, &[]),
            Err(why) => row(item, "blocked", format!("{why}; needs_image_layer {name}"), Some("halti-owner"), &[]),
        });
    }
    for (id, file) in m.detected(&ws.root, &uses) {
        states.push(row(&id, "suggested", format!("{file} found"), Some("agent"), &["use", &id]));
    }
    let mut held = Vec::new();
    for u in uses.iter().filter(|_| !trusted) {
        let servers = m.exts.get(&u.id).into_iter().flat_map(|e| &e.0.servers);
        held.extend(servers.filter(|s| u.team || code.iter().any(|c| c.0 == s.id)).map(|s| s.id.clone()));
    }
    let launches = launches(&cx, &m, &uses, &roots, &ok, &held, trusted);
    let mut notes = m.notes.clone();
    notes.push(match &m.dev {
        Some((path, _)) => format!("layer: {} for repo {}", path.display(), ws.repo_name().display()),
        None => "layer: none; 'itse use <ext>' declares one".into(),
    });
    notes.extend(m.team.iter().map(|_| "team layer: .itse/itse.json, read-only".to_string()));
    Ok(Resolved { manifest: m, states, launches, notes, trust_hash, trusted, held, roots })
}

/// One package's row, and its root when it is present.
fn package(cx: &Cx, ext: &str, name: &str, pkg: &Package, install: &str) -> Result<(State, Option<PathBuf>)> {
    if install == "path" {
        let bin = pkg.bin.keys().next().map_or(name, String::as_str);
        let Some(path) = which(bin, cx.path) else {
            let (actor, next): (_, &[&str]) = if pkg.pin_source.is_some() { ("agent", &["use", ext, "install=fetch"]) } else { ("halti-owner", &[]) };
            return Ok((row(name, "missing", format!("{bin} not on PATH"), Some(actor), next), None));
        };
        let found = lock::found(cx.dirs, name, &path)?;
        // The root is where the bin's path starts (a JDK's, above `bin/java`), past links like /usr/bin/java.
        let (real, rel) = (fs::canonicalize(&path).unwrap_or_else(|_| path.clone()), Path::new(pkg.bin.get(bin).map_or(bin, String::as_str)));
        let root = real.ancestors().nth(rel.components().count()).filter(|_| real.ends_with(rel)).or(path.parent()).map(Path::to_path_buf);
        return Ok((row(name, "unpinned", format!("found {} (sha256 {}…)", path.display(), &found.sha256[..12]), None, &[]), root));
    }
    let hosts = pkg.hosts.clone();
    let locked = cx.lock.packages.get(name);
    let Some((locked, pin)) = locked.and_then(|l| Some((l, l.targets.get(target())?))) else {
        if cx.waiting.iter().any(|w| w == name) {
            return Ok((row(name, "untrusted", "pinned only by the team lock .itse/itse.lock.json", Some("human"), &["trust"]), None));
        }
        return Ok((State { hosts, ..row(name, "missing", format!("no pin for {}", target()), Some("agent"), &["lock", "--refresh", name]) }, None));
    };
    let item = format!("{name} {}", locked.purl.rsplit_once('@').map_or("", |v| v.1)).trim_end().to_string();
    let root = cx.dirs.data.join("store").join(format!("sha256-{}", pin.sha256));
    if !root.is_dir() {
        let size = pin.size.map_or(String::new(), |s| format!("; {} MB", s / 1_000_000));
        return Ok((State { hosts, ..row(item, "missing", format!("not in store{size}"), Some("agent"), &["install", name]) }, None));
    }
    let consents = lock::missing_consents(&cx.consents, name, &pkg.consents, Some(&root));
    let state = match locked.expires_estimate.as_deref() {
        Some(est) if est < cx.today.as_str() => row(item, "expired", format!("pin estimated to expire {est}"), Some("agent"), &["lock", "--refresh", name]),
        _ if !consents.is_empty() => row(item, "needs_consent", consents.join(", "), Some("human"), &["consent", name]),
        Some(est) if est <= cx.soon.as_str() => row(item, "expiring", format!("pin estimated to expire {est}"), Some("agent"), &["lock", "--refresh", name]),
        _ => row(item, "ready", format!("store sha256-{}…", &pin.sha256[..pin.sha256.len().min(12)]), None, &[]),
    };
    Ok((state, Some(root)))
}

/// A `Launch` for each server of the extensions in use whose packages are present and usable,
/// leaving out what runs project code (and everything from the team layer) until trust. Templates
/// in `init_options` and `settings` are rendered like the command's; a server without
/// `init_options` is initialized with its settings under its own id.
fn launches(cx: &Cx, m: &Manifest, uses: &[Use], roots: &BTreeMap<String, PathBuf>, ok: &BTreeSet<String>, held: &[String], trusted: bool) -> Vec<Launch> {
    let (ws, dirs, consents) = (cx.ws, cx.dirs, &cx.consents);
    let settings = m.merged(|l| &l.servers);
    let mut out = Vec::new();
    for u in uses {
        for s in m.exts.get(&u.id).into_iter().flat_map(|e| &e.0.servers) {
            if held.contains(&s.id) {
                continue;
            }
            let mut fill = |key: &str| match key.split_once(':') {
                Some(("pkg", p)) => roots.get(p).filter(|_| ok.contains(p)).map(|r| r.display().to_string()),
                Some(("consent", id)) => uses.iter().flat_map(|u| m.exts.get(&u.id)).flat_map(|e| &e.0.packages).find_map(|(name, p)| {
                    let spec = p.consents.iter().find(|c| c.id == id)?;
                    Some(format!("{}{}", spec.flag.as_deref().unwrap_or(""), lock::consented(consents.get(name)?, spec)?))
                }),
                _ if key == "cache" => Some(dirs.cache.display().to_string()),
                _ if key == "workspaceFolder" => Some(ws.root.display().to_string()),
                _ if key == "ws" => Some(ws.ws.clone()),
                _ => None,
            };
            let argv: Result<Vec<String>> = s.command.iter().map(|t| render(t, &mut fill)).collect();
            let env: Result<Vec<(String, String)>> = s.env.iter().map(|(k, v)| Ok((k.clone(), render(v, &mut fill)?))).collect();
            let via = match &s.via {
                Some(DapVia::Tcp { argv, port }) => argv.iter().map(|t| render(t, &mut fill)).collect::<Result<_>>().map(|argv| Some(DapVia::Tcp { argv, port: *port })),
                via => Ok(via.clone()),
            };
            let (Ok(argv), Ok(env), Ok(via)) = (argv, env, via) else { continue };
            // Before trust the team layer is read for status only, so its server settings wait too.
            let set = settings.get(&s.id).filter(|x| trusted || !x.1).map(|x| x.0);
            // Exposing a server's tools is the developer's choice alone, never the team layer's.
            let exposed = settings.get(&s.id).filter(|x| !x.1 && x.0.expose).map(|x| x.0);
            let lsp_settings = set.and_then(|x| x.settings.as_ref()).unwrap_or(&s.settings);
            let (Ok(settings), Ok(init_options)) = (render_value(lsp_settings, &mut fill), render_value(&s.init_options, &mut fill)) else { continue };
            // Without init_options a server gets its own section of the settings, so a manifest says them once.
            let init_options = if s.init_options.is_null() { settings.get(&s.id).cloned().unwrap_or_default() } else { init_options };
            let exit_codes = m.packages_of(s, uses).flat_map(|p| &p.exit_codes).filter_map(|(k, v)| Some((k.parse().ok()?, v.clone()))).collect();
            out.push(Launch {
                id: s.id.clone(),
                protocol: s.protocol,
                argv,
                cwd: ws.root.clone(),
                env,
                limits: s.limits.clone(),
                exit_codes,
                settings,
                init_options,
                languages: s.languages.clone(),
                filetypes: s.filetypes.clone(),
                outputs: s.outputs.clone(),
                idle_minutes: set.and_then(|x| x.idle_minutes).or(s.idle_minutes),
                timeout_s: s.timeout_s,
                expose: exposed.is_some(),
                tools: exposed.and_then(|x| x.tools.clone()),
                max_list_bytes: exposed.and_then(|x| x.max_list_bytes),
                via,
            });
        }
    }
    out
}

/// The first executable named `bin` in the caller's PATH (absolute entries only).
pub fn which(bin: &str, path: Option<&String>) -> Option<PathBuf> {
    let dirs = path.into_iter().flat_map(|p| p.split(':')).filter(|d| d.starts_with('/'));
    dirs.map(|d| Path::new(d).join(bin)).find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// A typed check of what only the image provides: a library in the usual dirs, or an argv that
/// must exit 0 within 5 s (run from `/`, its output dropped).
fn system_check(c: &SystemCheck) -> Result<(), String> {
    if let Some(lib) = &c.lib {
        let multiarch = format!("/usr/lib/{}-linux-gnu", env::consts::ARCH);
        if ![multiarch.as_str(), "/lib", "/usr/lib", "/lib64", "/usr/lib64", "/usr/local/lib"].iter().any(|d| Path::new(d).join(lib).exists()) {
            return Err(format!("{lib} not found"));
        }
    }
    let Some((prog, args)) = c.cmd.split_first() else { return Ok(()) };
    let cmd = c.cmd.join(" ");
    let mut child = Command::new(prog).args(args).current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("{cmd}: {e}"))?;
    // try_wait keeps a status once it has one, so asking again after the wait is free.
    sys::wait_until(Duration::from_secs(5), || !matches!(child.try_wait(), Ok(None)));
    match child.try_wait() {
        Ok(Some(status)) if status.success() => Ok(()),
        Ok(Some(status)) => Err(format!("{cmd} exit {}", status.code().unwrap_or(-1))),
        Ok(None) => {
            let _ = (child.kill(), child.wait());
            Err(format!("{cmd} took over 5 s"))
        }
        // Such as a child reaped elsewhere, when itse was started with SIGCHLD ignored.
        Err(e) => Err(format!("{cmd}: {e}")),
    }
}

impl Resolved {
    /// `itse consent <pkg> …` against the package's declared consents and its installed files.
    pub fn consent(&self, dirs: &Dirs, args: &Args, tty: bool) -> Result<Outcome> {
        let pkg = args.target.as_deref().unwrap_or_default();
        let spec = self.manifest.package(pkg).ok_or_else(|| anyhow!("no package '{pkg}' in any extension"))?;
        lock::consent(dirs, pkg, &spec.consents, self.roots.get(pkg).map(PathBuf::as_path), &args.flags, tty)
    }
}

#[cfg(test)]
mod tests;
