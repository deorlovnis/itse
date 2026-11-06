//! Harness glue, all of it from extension data (`integration`, `hooks`, `identity`): the core names
//! no harness of its own.
//!
//! `itse integrate <ext>` is the only writer of harness files and human-only. Without `--yes` it
//! says what it would change; with it, it writes the skill, merges a hook recipe into a JSON file
//! by JSON pointer, keeping every other key, and runs the harness's own command to register the
//! MCP endpoint, so a file the harness rewrites while it runs has one writer. Targets outside the
//! home, inside a git work tree or a workspace root, files that aren't valid JSON or not of the
//! recipe's shape, and folders the harness's command would write into on those terms are refused,
//! and then nothing changes. `--remove` takes back exactly what it added: a merged file unchanged
//! since goes back byte for byte (the merge writes keys sorted), else only the entry goes.
//!
//! `itse hook <recipe>` answers a harness hook from its stdin. It never starts the daemon or a
//! server: it asks a running daemon about the edited file's workspace, prints the recipe's `wrap`
//! only for an answer that settled with something new, and always exits 0.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Args, Tool, VERBS, synopsis};
use crate::daemon::client;
use crate::manifest::{self, Manifest};
use crate::results::{HOOK, Outcome, State, Verdict, clip, text};
use crate::workspace::{Dirs, Workspace};
use crate::{lock, resolve, sys};

/// A hook's stdin is read up to this; a bigger one gets no answer.
const MAX_INPUT: u64 = 1 << 20;
/// The whole hook call, settle included, waits this long for the daemon at most.
const HOOK_WAIT: Duration = Duration::from_secs(8);

/// An extension's `integration`: the harness's config folder, its skill, its MCP registration and
/// its hook recipes.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Integration {
    config: Option<ConfigSpec>,
    skill: Option<SkillSpec>,
    mcp: Option<McpSpec>,
    recipes: BTreeMap<String, Recipe>,
}

/// The env var that moves the harness's files, and where each declared path (`~/…`) then goes in
/// the folder it names (`""`: the folder itself). Without `--home` they go there when it is set,
/// as the harness reads them; with `--home` its command runs without it, so `~` at DIR decides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSpec {
    env: String,
    paths: BTreeMap<String, String>,
}

/// Where the skill goes (`~/…`) and its front matter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillSpec {
    path: String,
    name: String,
    description: String,
}

/// The harness's commands that add and remove the endpoint; `${json}` is `value`, run in `cwd`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSpec {
    add: Vec<String>,
    remove: Vec<String>,
    cwd: String,
    value: Value,
    /// What the commands write (`~/…`), refused on the same terms as a skill or a recipe target.
    writes: Vec<String>,
    /// What the harness says when it removes an entry that is gone already.
    absent: String,
    /// What it says when it adds an entry that is there already.
    exists: String,
}

/// `append` goes into the array at `pointer` in the JSON file `file` (`~/…`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    file: String,
    pointer: String,
    append: Value,
}

/// An entry of `hooks`: where the edited file and the cwd are in stdin's JSON, the daemon verb and
/// flags it asks with the file as the target, and the JSON it prints, `${text}` being the answer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSpec {
    path_from: String,
    cwd_from: String,
    verb: String,
    flags: BTreeMap<String, Value>,
    wrap: Value,
    /// `empty`: say nothing when the answer is EMPTY.
    quiet_when: Option<String>,
}

/// One change `integrate` makes: what it would do, its key in `integrations.json` (with the sha256
/// of the file it writes, if any, and a key of its own: for a merge, `before`, the file's text
/// before it, null if it made the file; for a skill, `made`, the folders it made), and the doing,
/// which says what it did.
struct Step<'a> {
    what: String,
    key: String,
    sha256: Option<String>,
    before: Option<(&'static str, Value)>,
    run: Box<dyn FnOnce() -> Result<String> + 'a>,
}

/// `itse integrate <ext> [--skill] [--mcp] [--recipe R] [--remove] [--home DIR] [--yes]`.
pub fn integrate(ws: &Workspace, dirs: &Dirs, env: &BTreeMap<String, String>, cwd: &Path, args: &Args, tty: bool) -> Result<Outcome> {
    let id = args.target.clone().unwrap_or_default();
    let m = Manifest::load(ws, dirs)?;
    let ext = &m.exts.get(&id).ok_or_else(|| anyhow!("no extension '{id}'; 'itse ext list' lists them"))?.0;
    let spec: Integration =
        serde_json::from_value(ext.integration.clone().ok_or_else(|| anyhow!("{id} declares no integration"))?).with_context(|| format!("{id}'s integration"))?;
    let (flag, recipe) = (|k: &str| args.flags.contains_key(k), args.flags.get("recipe").and_then(Value::as_str));
    let (yes, remove, actor) = (flag("yes"), flag("remove"), if tty { "tty" } else { "non-tty" });
    ensure!(flag("skill") || flag("mcp") || recipe.is_some(), "nothing to do; integrate takes --skill, --mcp or --recipe R");
    let given = args.flags.get("home").and_then(Value::as_str);
    let user_home = env.get("HOME").and_then(|h| fs::canonicalize(h).ok());
    let home = given.map(|h| cwd.join(h)).or_else(|| env.get("HOME").map(PathBuf::from)).unwrap_or_default();
    let home = fs::canonicalize(&home).with_context(|| format!("home {}", home.display()))?;
    // The harness's files are where its config env says, unless `--home` puts `~` elsewhere.
    let moved = spec.config.as_ref().filter(|_| given.is_none()).and_then(|c| Some((c, cwd.join(env.get(&c.env).filter(|d| !d.is_empty())?))));
    let place = |declared: &str| place(declared, moved.as_ref());
    let roots = known_roots(ws, dirs, user_home.as_deref());
    let (mut steps, mut states) = (Vec::new(), Vec::new());
    if flag("skill") {
        let s = spec.skill.as_ref().ok_or_else(|| anyhow!("{id} declares no skill"))?;
        let path = target(&home, &place(&s.path), &roots)?;
        let (text, key) = (skill(&s.name, &s.description), format!("{id} skill {}", path.display()));
        let (sha256, recorded) = (Some(sys::sha256_hex(text.as_bytes())), records(dirs)?.remove(&key));
        // The skill's folder and the skills folder above it, those itse made: only they go with it.
        let mut made: Vec<String> = recorded.as_ref().and_then(|r| serde_json::from_value(r["made"].clone()).ok()).unwrap_or_default();
        made.extend(path.ancestors().skip(1).take(2).filter(|d| !d.exists()).map(|d| d.display().to_string()));
        made.sort();
        made.dedup();
        steps.push(match remove {
            true => {
                let (what, recorded) = (format!("remove {}", path.display()), recorded.map(|r| r["sha256"].clone()));
                Step { what, key, sha256, before: None, run: Box::new(move || remove_skill(&path, &text, recorded, &made)) }
            }
            false => {
                let what = format!("write {} ({} lines)", path.display(), text.lines().count());
                Step { what, key, sha256, before: Some(("made", json!(made))), run: Box::new(move || write_skill(&path, &text)) }
            }
        });
    }
    if let Some(name) = recipe {
        let r = spec.recipes.get(name).ok_or_else(|| anyhow!("{id} has no recipe '{name}'; it has: {}", spec.recipes.keys().cloned().collect::<Vec<_>>().join(", ")))?;
        let path = target(&home, &place(&r.file), &roots)?;
        let (shown, key) = (format!("{} at {}", path.display(), r.pointer), format!("{id} recipe {name} {}", path.display()));
        let recorded = records(dirs)?.remove(&key).unwrap_or_default();
        // Merged now, so a file that isn't JSON, or not of the recipe's shape, refuses the whole call
        // before anything changes.
        let (before, doc) = read_json(&path)?;
        let doc = merged(doc, &shown, &r.pointer, &r.append, remove)?;
        let unchanged = before.as_ref().is_some_and(|b| recorded["sha256"].as_str() == Some(sys::sha256_hex(b.as_bytes()).as_str()));
        let step = match (remove, recorded.get("before").cloned()) {
            // Still as itse wrote it: the file goes back to what it was, byte for byte.
            (true, Some(was)) if unchanged => {
                let (what, done) = match was.is_null() {
                    true => (format!("remove {}, which itse made", path.display()), format!("removed {}, which itse made", path.display())),
                    false => (format!("restore {} as it was", path.display()), format!("restored {} as it was", path.display())),
                };
                Step { what, key, sha256: None, before: None, run: Box::new(move || restore(&path, &was).map(|()| done)) }
            }
            (true, was) => {
                let made = was.is_some_and(|w| w.is_null());
                let what = format!("remove from {shown}: {}", r.append);
                Step { what, key, sha256: None, before: None, run: Box::new(move || put(&path, &shown, doc.as_ref().map(bytes), true, made)) }
            }
            (false, _) => {
                let sorted = if before.is_some() && doc.is_some() { " (the file is written with its keys sorted)" } else { "" };
                let what = format!("append to {shown}: {}{sorted}", r.append);
                let written = doc.as_ref().map(bytes);
                // Nothing to write keeps what was recorded when itse wrote it.
                let (sha256, before) = match &written {
                    Some(b) => (Some(sys::sha256_hex(b)), Some(before.map_or(Value::Null, Value::String))),
                    None => (recorded["sha256"].as_str().map(String::from), recorded.get("before").cloned()),
                };
                let before = before.map(|b| ("before", b));
                Step { what, key, sha256, before, run: Box::new(move || put(&path, &shown, written, false, false)) }
            }
        };
        steps.push(step);
    }
    if flag("mcp") {
        let s = spec.mcp.as_ref().ok_or_else(|| anyhow!("{id} declares no mcp"))?;
        for declared in &s.writes {
            target(&home, &place(declared), &roots)?;
        }
        let value = s.value.to_string();
        let argv: Vec<String> = if remove { &s.remove } else { &s.add }.iter().map(|t| t.replace("${json}", &value)).collect();
        let dir = PathBuf::from(s.cwd.replace("${workspaceFolder}", &ws.root.to_string_lossy()));
        let (line, key) = (crate::mcp::shell_line(&argv), format!("{id} mcp {}", dir.display()));
        // `--home` drops the config env from the command's, so it writes under DIR too.
        let unset = spec.config.as_ref().filter(|_| given.is_some()).map(|c| c.env.clone());
        let seen = match &moved {
            Some((c, d)) => format!("HOME {}, {} {}", home.display(), c.env, d.display()),
            None => format!("HOME {}", home.display()),
        };
        // A removal takes back only an add of itse's, and is done if the entry is gone already. An
        // add itse made is made again, in case a human removed it since, and done if it is there.
        let ours = records(dirs)?.contains_key(&key);
        let done_if = match (remove, ours) {
            (true, _) => Some((s.absent.clone(), "gone already")),
            (false, true) => Some((s.exists.clone(), "there already")),
            (false, false) => None,
        };
        if remove && !ours {
            let why = "itse never added it";
            let (what, done) = (format!("skip {line} in {}: {why}", dir.display()), format!("skipped {line}: {why}"));
            steps.push(Step { what, key, sha256: None, before: None, run: Box::new(move || Ok(done)) });
        } else {
            match argv.first().and_then(|bin| resolve::which(bin, env.get("PATH"))) {
                // Nothing to run it with: the human runs it, in the same folder.
                None => states.push(State {
                    item: "mcp".into(),
                    state: "missing".into(),
                    reason: Some(format!("{} is not on PATH; a human runs, in {}: {line}", argv.first().map_or("", String::as_str), dir.display())),
                    actor: Some("human".into()),
                    ..State::default()
                }),
                Some(bin) => steps.push(Step {
                    what: format!("run in {} ({seen}): {line}", dir.display()),
                    key,
                    sha256: None,
                    before: None,
                    run: Box::new(move || run(&bin, &argv[1..], &dir, &home, unset.as_deref(), done_if, &line)),
                }),
            }
        }
    }
    let (mut notes, mut done, mut failed) = (Vec::new(), 0, 0);
    for step in steps {
        if !yes {
            notes.push(format!("would {}", step.what));
            continue;
        }
        let mut entry = (!remove).then(|| json!({"actor": actor, "at": sys::utc(SystemTime::now()), "sha256": step.sha256}));
        if let (Some(entry), Some((name, value))) = (&mut entry, step.before) {
            entry[name] = value;
        }
        match (step.run)().and_then(|said| record(dirs, &step.key, entry).map(|()| said)) {
            Ok(said) => {
                done += 1;
                notes.push(said);
            }
            Err(e) => {
                failed += 1;
                notes.push(format!("failed to {}: {e:#}", step.what));
            }
        }
    }
    if !yes && !notes.is_empty() {
        let tokens = ["integrate", &id].into_iter().map(String::from).chain(args.flags.iter().map(|(k, v)| match v.as_str() {
            Some(v) => format!("--{k}={v}"),
            None => format!("--{k}"),
        }));
        let next = Some(tokens.chain(["--yes".to_string()]).collect());
        states.push(State {
            item: id.clone(),
            state: "planned".into(),
            reason: Some("nothing changes without --yes".into()),
            actor: Some("human".into()),
            next,
            ..State::default()
        });
    }
    let verdict = match () {
        _ if failed > 0 => Verdict::Fail,
        _ if states.iter().any(|s| s.state == "missing") => Verdict::NeedsHuman,
        _ => Verdict::Ok,
    };
    let counts = match () {
        _ if !yes => format!("{} planned", notes.len()),
        _ if failed > 0 => format!("{done} done by {actor}, {failed} failed"),
        _ => format!("{done} done by {actor}"),
    };
    Ok(Outcome { notes, states, err: (verdict == Verdict::NeedsHuman).then_some("needs_human"), ..Outcome::new("integrate", verdict, id, counts) })
}

/// The SKILL.md of a harness: the same in every workspace, since what differs (actions, extension
/// notes, debug) is one `itse verbs` away, and made from the verb table, so it never drifts from
/// the CLI. It lists the verbs the MCP tools carry; front matter values are quoted for YAML.
pub fn skill(name: &str, description: &str) -> String {
    let verbs = VERBS.iter().filter(|v| matches!(v.tool, Tool::Run | Tool::Query | Tool::Show));
    let lines: String = verbs.map(|v| format!("- `{}`: {}\n", format!("itse {} {}", v.name, synopsis(v)).trim_end(), v.about)).collect();
    text("skill", &[("name", &json!(name)), ("description", &json!(description)), ("verbs", &lines)])
}

/// What `integrate` did, by step, in `integrations.json`: who (`tty` or `non-tty`), when, and the
/// sha256 of a file it wrote, so `--remove` takes back only that.
fn records(dirs: &Dirs) -> Result<BTreeMap<String, Value>> {
    lock::read(&dirs.state.join("integrations.json"))
}

fn record(dirs: &Dirs, key: &str, entry: Option<Value>) -> Result<()> {
    let mut all = records(dirs)?;
    match entry {
        Some(entry) => all.insert(key.into(), entry),
        None => all.remove(key),
    };
    lock::write(&dirs.state.join("integrations.json"), &all)
}

/// The workspace roots itse knows: this one and every one that ever had a daemon, but for one
/// without git at or above the user's home, so an `itse` call in `~` doesn't make all of it one.
fn known_roots(ws: &Workspace, dirs: &Dirs, user_home: Option<&Path>) -> Vec<PathBuf> {
    let above = |r: &PathBuf| !r.join(".git").exists() && user_home.is_some_and(|h| h.starts_with(r));
    [ws.root.clone()].into_iter().chain(recorded_roots(dirs)).filter(|r| !above(r)).collect()
}

/// The roots of the workspaces that ever had a daemon.
fn recorded_roots(dirs: &Dirs) -> Vec<PathBuf> {
    fs::read_dir(dirs.state.join("ws")).into_iter().flatten().flatten().filter_map(|d| fs::read_to_string(d.path().join("root")).ok()).map(PathBuf::from).collect()
}

/// `declared` (`~/…`) where the harness keeps it: in the folder its config env names, if `moved`.
fn place(declared: &str, moved: Option<&(&ConfigSpec, PathBuf)>) -> String {
    let Some((config, dir)) = moved else { return declared.into() };
    let hit = config.paths.iter().find_map(|(from, to)| Some((declared.strip_prefix(from.as_str()).filter(|r| r.is_empty() || r.starts_with('/'))?, to)));
    match hit {
        Some((rest, to)) if to.is_empty() => format!("{}{rest}", dir.display()),
        Some((rest, to)) => format!("{}{rest}", dir.join(to).display()),
        None => declared.into(),
    }
}

/// A harness file at `declared` (`~/…`, with `~` at `home`), unless it is outside the home or inside
/// a git work tree or a workspace root, where itse writes nothing. Its deepest existing folder is
/// checked as it really is, so a link can't lead out of the home or into a repo; a linked file is
/// the file it names, so a write lands there and the link stays, and a dangling link is refused.
fn target(home: &Path, declared: &str, roots: &[PathBuf]) -> Result<PathBuf> {
    let path = home.join(declared.strip_prefix("~/").unwrap_or(declared));
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) && !path.exists() {
        bail!("{} is a link to a missing file; not changed", path.display());
    }
    let real = path.ancestors().find_map(|a| fs::canonicalize(a).ok()).unwrap_or_default();
    if path.components().any(|c| c == Component::ParentDir) || !real.starts_with(home) {
        bail!("{} is outside {}; not changed", path.display(), home.display());
    }
    if let Some(tree) = real.ancestors().find(|a| a.join(".git").exists() || roots.iter().any(|r| r == a)) {
        bail!("{} is inside {}, a work tree or workspace; not changed", path.display(), tree.display());
    }
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

fn write_skill(path: &Path, text: &str) -> Result<String> {
    write_file(path, text.as_bytes())?;
    Ok(format!("wrote {}", path.display()))
}

/// Write `path` whole, its folders made, keeping the mode of the file it replaces.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mode = fs::metadata(path).ok().map(|m| m.permissions());
    fs::create_dir_all(path.parent().unwrap_or(path))?;
    sys::write_atomic(path, bytes)?;
    if let Some(mode) = mode {
        fs::set_permissions(path, mode)?;
    }
    Ok(())
}

/// Remove the skill if it is what itse wrote (as recorded) or would write now; a skill changed
/// since is kept.
fn remove_skill(path: &Path, text: &str, recorded: Option<Value>, made: &[String]) -> Result<String> {
    let Ok(bytes) = fs::read(path) else { return Ok(format!("{} is not there", path.display())) };
    let sha = sys::sha256_hex(&bytes);
    ensure!(bytes == text.as_bytes() || recorded.is_some_and(|r| r == sha.as_str()), "{} changed since itse wrote it; not removed", path.display());
    fs::remove_file(path)?;
    // Its folder and the skills folder above it go too, each once empty, if itse made them.
    for dir in path.ancestors().skip(1).take(2).filter(|d| made.iter().any(|m| Path::new(m) == *d)) {
        let _ = fs::remove_dir(dir);
    }
    Ok(format!("removed {}", path.display()))
}

/// A JSON file's text and value; None and `{}` if it doesn't exist yet.
fn read_json(path: &Path) -> Result<(Option<String>, Value)> {
    let invalid = || anyhow!("{} is not valid JSON; not changed", path.display());
    match fs::read_to_string(path) {
        Ok(text) => {
            let doc = serde_json::from_str(&text).map_err(|_| invalid())?;
            Ok((Some(text), doc))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((None, json!({}))),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(invalid()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `doc` with `entry` appended to the array at `pointer`, made with the objects above it if missing,
/// unless an entry deep-equal to it is there; or with `remove`, without the entries deep-equal to it
/// and the containers that leaves empty. Every other key stays; None if nothing changes. `shown`
/// names the place in an error.
fn merged(mut doc: Value, shown: &str, pointer: &str, entry: &Value, remove: bool) -> Result<Option<Value>> {
    let keys: Vec<String> = pointer.split('/').skip(1).map(|k| k.replace("~1", "/").replace("~0", "~")).collect();
    let changed = if remove {
        let list = doc.pointer_mut(pointer).and_then(Value::as_array_mut);
        let before = list.as_ref().map_or(0, |l| l.len());
        let after = list.map_or(0, |l| {
            l.retain(|e| e != entry);
            l.len()
        });
        if before != after {
            prune(&mut doc, &keys);
        }
        before != after
    } else {
        let mut at = &mut doc;
        for (i, key) in keys.iter().enumerate() {
            let empty = if i + 1 == keys.len() { json!([]) } else { json!({}) };
            at = at.as_object_mut().ok_or_else(|| anyhow!("{shown}: not an object above it; not changed"))?.entry(key).or_insert(empty);
        }
        let list = at.as_array_mut().ok_or_else(|| anyhow!("{shown}: not an array; not changed"))?;
        let new = !list.contains(entry);
        if new {
            list.push(entry.clone());
        }
        new
    };
    Ok(changed.then_some(doc))
}

/// A merged document as `put` writes it. serde_json keeps no key order, so its keys are sorted.
fn bytes(doc: &Value) -> Vec<u8> {
    format!("{doc:#}\n").into_bytes()
}

/// Write a merged document to `path`, or say why there is nothing to write. A removal that leaves
/// `{}` in a file itse `made` removes the file.
fn put(path: &Path, shown: &str, bytes: Option<Vec<u8>>, remove: bool, made: bool) -> Result<String> {
    let Some(bytes) = bytes else { return Ok(format!("{shown}: {}", if remove { "no such entry" } else { "already there" })) };
    match made && bytes == b"{}\n" {
        true => fs::remove_file(path)?,
        false => write_file(path, &bytes)?,
    }
    Ok(format!("{} {shown}", if remove { "removed from" } else { "appended to" }))
}

/// Put back the file a merge changed: its text `was`, or with null, no file.
fn restore(path: &Path, was: &Value) -> Result<()> {
    match was.as_str() {
        Some(text) => write_file(path, text.as_bytes()),
        None => Ok(fs::remove_file(path)?),
    }
}

/// Remove the value at `keys` if it is an empty array or object, then its parent the same way.
fn prune(doc: &mut Value, keys: &[String]) {
    let Some((last, parents)) = keys.split_last() else { return };
    let Some(parent) = parents.iter().try_fold(&mut *doc, |at, k| at.get_mut(k)).and_then(Value::as_object_mut) else { return };
    if parent.get(last).is_some_and(|v| v.as_array().is_some_and(Vec::is_empty) || v.as_object().is_some_and(Map::is_empty)) {
        parent.remove(last);
        prune(doc, parents);
    }
}

/// Run the harness's command in `dir` with `~` at `home` and without the env var `unset`; what it
/// said. A failure that says the first of `done_if` (an entry removed or added already) is done
/// too, as its second says.
fn run(bin: &Path, args: &[String], dir: &Path, home: &Path, unset: Option<&str>, done_if: Option<(String, &str)>, line: &str) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).current_dir(dir).env("HOME", home).stdin(Stdio::null());
    if let Some(var) = unset {
        cmd.env_remove(var);
    }
    let out = cmd.output().with_context(|| format!("running {line}"))?;
    let said = [&out.stdout, &out.stderr].map(|o| String::from_utf8_lossy(o).trim().to_string()).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ");
    if let Some((_, done)) = done_if.filter(|(words, _)| !out.status.success() && said.contains(words.as_str())) {
        return Ok(format!("ran {line}: {said}; {done}"));
    }
    ensure!(out.status.success(), "exited {}: {said}", out.status.code().unwrap_or(-1));
    Ok(format!("ran {line}: {said}"))
}

/// `itse hook <recipe>`: whatever happens, exit 0, with the answer on stdout or nothing.
pub fn hook(recipe: &str) -> u8 {
    if let Ok(Some(out)) = hook_answer(recipe) {
        // A harness that stopped reading gets nothing, and still exit 0.
        let _ = writeln!(io::stdout(), "{out}");
    }
    0
}

/// The hook's output, if it has any: the recipe's `wrap` around the daemon's answer, for an answer
/// that settled and isn't quiet. The baseline key is stdin's session field (the daemon hashes it),
/// so a session's first settled call about a file has no baseline for it yet and gives what the
/// file holds as new, as a first `problems --new` does.
fn hook_answer(name: &str) -> Result<Option<String>> {
    let mut input = Vec::new();
    io::stdin().take(MAX_INPUT + 1).read_to_end(&mut input)?;
    ensure!(input.len() as u64 <= MAX_INPUT, "stdin is over 1 MB");
    let payload: Value = serde_json::from_slice(&input)?;
    let dirs = Dirs::from_env(|k| std::env::var_os(k))?;
    let (exts, _) = manifest::extensions(&dirs)?;
    let found = exts.values().find_map(|(e, _)| Some((e, e.hooks.as_ref()?.get(name)?.clone())));
    let (ext, spec) = found.ok_or_else(|| anyhow!("no extension declares the hook '{name}'"))?;
    let spec: HookSpec = serde_json::from_value(spec)?;
    let field = |p: &str| payload.pointer(p).and_then(Value::as_str).map(PathBuf::from);
    // A relative path is the payload's cwd's; joining an absolute one keeps it.
    let (Some(file), Some(cwd)) = (field(&spec.path_from), field(&spec.cwd_from)) else { return Ok(None) };
    let file = fs::canonicalize(cwd.join(file))?;
    let Some(root) = hook_root(&file, &dirs) else { return Ok(None) };
    let ws = Workspace::find(Some(root), &file)?;
    let rel = file.strip_prefix(&ws.root)?.to_string_lossy().into_owned();
    // Nothing declared that could answer: no server and no action in use.
    let m = Manifest::load(&ws, &dirs)?;
    if !m.uses().iter().filter_map(|u| m.exts.get(&u.id)).any(|(e, _)| !e.servers.is_empty() || !e.actions.is_empty()) {
        return Ok(None);
    }
    let session = ext.identity.as_ref().and_then(|i| payload.pointer(i.hook_session_from.as_deref()?)?.as_str()).unwrap_or("");
    let (mut conn, _) = client::connect(&ws, &dirs, "hook", &[("ITSE_SESSION", session)])?;
    let call = json!({"verb": spec.verb, "args": {"target": rel, "flags": spec.flags}, "json": false, "page": 1});
    let reply = client::ask(&mut conn, "call", call, Some(HOOK_WAIT))?;
    if reply["settled"] != true || (spec.quiet_when.as_deref() == Some("empty") && reply["verdict"] == "empty") {
        return Ok(None);
    }
    // `${text}` stands inside a JSON string, so the text goes in escaped, without its quotes.
    let quoted = serde_json::to_string(&clip(reply["text"].as_str().unwrap_or(""), HOOK.1))?;
    Ok(Some(spec.wrap.to_string().replace("${text}", &quoted[1..quoted.len() - 1])))
}

/// The edited file's workspace: the deepest root holding it of those that ever had a daemon (one
/// without git, or `--root` below its git root, included) and its git work tree. None outside all
/// of them, and then no workspace has anything to say about the file.
fn hook_root(file: &Path, dirs: &Dirs) -> Option<PathBuf> {
    let git = file.ancestors().skip(1).find(|d| d.join(".git").exists()).map(Path::to_path_buf);
    recorded_roots(dirs).into_iter().chain(git).filter(|r| file.starts_with(r)).max_by_key(|r| r.components().count())
}

#[cfg(test)]
mod tests;
