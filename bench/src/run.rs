//! `itse-bench run`: each task × rep × arm is one trial in `<root>/<run>/<task>-<rep>-<arm>/` and one
//! line of `results.jsonl`. The arms differ in PATH alone: `with` puts `bin/` (only an `itse` link)
//! and the shim dir first; `without` is the bench's PATH with every dir that offers itse masked.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, thread};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Map, Value, json};

use crate::adapter::{self, Adapter};
use crate::report;
use crate::task::{self, Task};

pub struct Opts {
    pub tasks: PathBuf,
    pub harness: String,
    pub adapters: PathBuf,
    pub reps: u32,
    pub seed: u64,
    pub out: PathBuf,
    pub itse: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub max_turns: Option<u32>,
    pub root: PathBuf,
}

/// Once per run: the adapter, the itse under test and the versions every line records.
struct Run<'a> {
    o: &'a Opts,
    adapter: &'a Adapter,
    adapter_dir: String,
    itse: PathBuf,
    itse_version: String,
    harness_version: String,
    id: String,
    dir: PathBuf,
    base: BTreeMap<String, String>,
}

pub fn run(o: &Opts) -> Result<()> {
    let file = fs::canonicalize(&o.adapters).with_context(|| format!("adapters {}", o.adapters.display()))?;
    let adapters = adapter::load(&file)?;
    let names = adapters.keys().cloned().collect::<Vec<_>>().join(", ");
    let adapter = adapters.get(&o.harness).with_context(|| format!("no adapter [{}] in {}; it has {names}", o.harness, file.display()))?;
    let unset: Vec<&str> = adapter.require_env.iter().map(String::as_str).filter(|k| env::var_os(k).is_none_or(|v| v.is_empty())).collect();
    ensure!(unset.is_empty(), "[{}] needs {} in the bench's env: each trial's config is fresh and holds no login", o.harness, unset.join(", "));
    let tasks = task::list(&o.tasks)?.iter().map(|d| Task::load(d)).collect::<Result<Vec<_>>>()?;
    let mut base = base_env(env::vars_os());
    let path = base.get("PATH").cloned().unwrap_or_default();
    // Resolved once, so every `with` trial links the same binary.
    let itse = o.itse.clone().or_else(|| sh(&base, Path::new("/"), "command -v itse").map(PathBuf::from)).context("no itse on PATH; pass --itse BIN")?;
    let itse = fs::canonicalize(&itse).with_context(|| format!("itse {}", itse.display()))?;
    let first_line = |cmd: &mut Command| output(cmd).map(|text| text.lines().next().unwrap_or_default().to_string());
    let itse_version = first_line(Command::new(&itse).arg("--version"))?;
    let adapter_dir = file.parent().unwrap_or(Path::new("/")).display().to_string();
    let vars = BTreeMap::from([("adapter_dir", adapter_dir.clone())]);
    let argv = adapter.version_cmd.iter().map(|a| adapter::template(a, &vars)).collect::<Result<Vec<_>>>()?;
    let harness_version = first_line(Command::new(&argv[0]).args(&argv[1..]))?;
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let id = format!("{secs}-{}-{}", o.seed, process::id());
    fs::create_dir_all(&o.root).with_context(|| format!("creating {}", o.root.display()))?;
    // Claude Code and other harnesses read these in every dir above their workspace.
    let memory = fs::canonicalize(&o.root)?.ancestors().flat_map(|a| ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md", ".claude"].map(|f| a.join(f))).find(|p| p.exists());
    ensure!(memory.is_none(), "{} would reach every trial's agent; pass --trials DIR elsewhere", memory.unwrap_or_default().display());
    let dir = fs::canonicalize(&o.root)?.join(&id);
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    base.insert("PATH".into(), mask(&path, &itse, &dir.join("tools"))?);
    fs::create_dir_all(&o.out)?;
    let r = Run { o, adapter, adapter_dir, itse, itse_version, harness_version, id, dir, base };
    // Each task's `use` entries, tried in a scratch dir first, so a typo ends no run partway.
    let scratch = r.dir.join("ext-check");
    fs::create_dir_all(scratch.join("ws"))?;
    let (env, mut uses) = (trial_env(&r.base, &scratch, false), tasks.iter().flat_map(|t| t.bench.uses.iter().map(move |ext| (t, ext))));
    uses.try_for_each(|(t, ext)| r.itse(&env, &scratch, &["use", ext]).map(drop).with_context(|| format!("task {}", t.id)))?;
    let mut results = OpenOptions::new().create(true).append(true).open(o.out.join("results.jsonl"))?;
    let mut rng = if o.seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { o.seed };
    thread::spawn(|| (std::io::stdin().read(&mut [0]), STOP.store(true, Relaxed)));
    eprintln!("itse-bench: run {} with {} ({}) and {}; trials in {}", r.id, o.harness, r.harness_version, r.itse_version, r.dir.display());
    for task in &tasks {
        for rep in 1..=o.reps {
            let order = if xorshift(&mut rng) & 1 == 0 { ["with", "without"] } else { ["without", "with"] };
            for arm in order {
                let line = r.trial(task, rep, arm, order).with_context(|| format!("trial {} rep {rep} {arm}", task.id))?;
                writeln!(results, "{line}")?;
                eprintln!("itse-bench: {} rep {rep} {arm}: {} in {} ms", task.id, verdict(&line), line["wall_ms"]);
            }
        }
    }
    Ok(())
}

/// Set for a worker, whose stdin is a pipe from the process the user started; STOP once it closes.
pub const WORKER: &str = "ITSE_BENCH_WORKER";
static STOP: AtomicBool = AtomicBool::new(false);

pub fn stopped() -> bool {
    STOP.load(Relaxed)
}

/// This command again as a worker in a group of its own, which Ctrl-C misses. A signal that ends this
/// process closes the worker's stdin; it ends its trial as on a timeout, cleans up and exits 130.
pub fn supervise() -> Result<ExitCode> {
    let mut worker = Command::new(env::current_exe()?).args(env::args_os().skip(1)).env(WORKER, "1").process_group(0).stdin(Stdio::piped()).spawn()?;
    let _pipe = worker.stdin.take();
    Ok(ExitCode::from(worker.wait()?.code().map_or(1, |c| c as u8)))
}

/// A trial's outcome for the progress line: pass, a harness error, or a failure and its cause.
pub fn verdict(line: &Value) -> String {
    match (line["pass"] == true, report::harness_error(line), line["timed_out"] == true) {
        (true, ..) => "pass".into(),
        (_, true, _) => format!("harness error (exit {}; see harness.err)", line["exit"]),
        (.., timed_out) => format!("fail ({})", if timed_out { "timed out".into() } else { format!("verifier exit {}", line["check_exit"]) }),
    }
}

impl Run<'_> {
    fn trial(&self, task: &Task, rep: u32, arm: &str, order: [&str; 2]) -> Result<Value> {
        let with = arm == "with";
        let dir = self.dir.join(format!("{}-{rep}-{arm}", task.id));
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        ["ws", "state", "cfg", "bin", "data"].iter().try_for_each(|sub| fs::create_dir(dir.join(sub)))?;
        fs::write(dir.join("gitconfig"), "[user]\n\tname = itse-bench\n\temail = itse-bench@localhost\n")?;
        let env = trial_env(&self.base, &dir, with);
        // Setup and the verifier see no itse in either arm.
        let plain = trial_env(&self.base, &dir, false);
        // However setup or the harness ends, the trial's daemon and processes end with it.
        let ran = self.agent(task, arm, &dir, &env, &plain);
        let daemon_stop = if with { self.itse(&env, &dir, &["daemon", "stop"]).map(drop) } else { Ok(()) };
        let mut left = reap(&dir);
        ensure!(!stopped(), "interrupted");
        let (mut line, base) = ran?;
        daemon_stop?;

        let intact = || if task::sha(&task.dir)? == task.sha { Ok(()) } else { Err(anyhow!("the task dir {} changed during the trial", task.dir.display())) };
        intact()?;
        let check_env: BTreeMap<String, String> = plain.clone().into_iter().chain([("WORKSPACE".into(), dir.join("ws").display().to_string())]).collect();
        let test_sh = ["bash".as_ref(), OsStr::new("tests/test.sh")];
        let (check_exit, check_timed_out) = group(&mut command(&test_sh, &task.dir, &check_env, &dir.join("test.log"), None)?, task.verifier_timeout, true)?;
        intact()?;
        // The agent's .gitattributes may name filter drivers in its .git/config; info/attributes wins.
        let attributes = dir.join("ws/.git/info/attributes");
        let _ = fs::remove_file(&attributes);
        fs::create_dir_all(dir.join("ws/.git/info")).and_then(|()| OpenOptions::new().write(true).create_new(true).open(&attributes)?.write_all(b"* -filter\n"))?;
        let shortstat = git(&plain, &dir, &["diff", "--no-ext-diff", "--no-textconv", "--shortstat", &base])?;
        let untracked = git(&plain, &dir, &["ls-files", "--others", "--exclude-standard"])?.lines().count();
        left.extend(reap(&dir));
        ensure!(!stopped(), "interrupted");
        let pass = check_exit == 0 && !check_timed_out;
        let more = json!({"rep": rep, "seed": self.o.seed, "order": order, "check_exit": check_exit, "check_timed_out": check_timed_out, "pass": pass,
            "shortstat": shortstat, "untracked": untracked, "leftovers": left});
        line.extend(serde_json::from_value::<Map<String, Value>>(more)?);
        line.extend(adapter::fields(self.adapter.parser, &fs::read_to_string(dir.join("harness.out")).unwrap_or_default(), &self.adapter.fields));
        Ok(Value::Object(line))
    }

    /// Setup, the arm's checks and itse calls, and the harness: the line so far and the base commit.
    fn agent(&self, task: &Task, arm: &str, dir: &Path, env: &BTreeMap<String, String>, plain: &BTreeMap<String, String>) -> Result<(Map<String, Value>, String)> {
        let (with, ws) = (arm == "with", dir.join("ws"));
        let path = |p: &Path| p.display().to_string();
        let setup = task.dir.join("environment/setup.sh");
        let (exit, _) = group(&mut command(&["bash".as_ref(), setup.as_os_str(), ws.as_os_str()], &task.dir, plain, &dir.join("setup.log"), None)?, task.verifier_timeout, true)?;
        ensure!(exit == 0, "environment/setup.sh exited {exit}; see {}", dir.join("setup.log").display());
        let base = git(plain, dir, &["rev-parse", "HEAD"])?;
        with.then(|| symlink(&self.itse, dir.join("bin/itse"))).transpose()?;
        let found = sh(env, &ws, "command -v itse");
        let arm_clean = if with { found.as_deref() == Some(dir.join("bin/itse").to_str().unwrap_or_default()) } else { found.is_none() };
        let cargo = sh(env, &ws, "command -v cargo");
        let clean = arm_clean && cargo.as_ref().is_some_and(|c| with || !is_itse(Path::new(c), &self.itse));
        ensure!(clean, "the {arm} arm finds itse at {found:?} and cargo at {cargo:?}");
        let (mut skill, home) = (Value::Null, path(&dir.join("cfg")));
        if with {
            task.bench.uses.iter().try_for_each(|ext| self.itse(env, dir, &["use", ext]).map(drop))?;
            self.itse(env, dir, &["trust"])?;
            // The harness's skill under `home` (its `~`): true once installed; null with no extension to integrate, or no integrate.
            if let Some(ext) = &self.adapter.integrate {
                let integrated = self.itse(env, dir, &["integrate", ext, "--skill", "--home", &home, "--yes"]);
                let missing = integrated.as_ref().is_err_and(|e| ["not built yet", "unknown verb"].iter().any(|m| e.to_string().contains(m)));
                skill = if missing { Value::Null } else { integrated.map(|_| json!(true))? };
            }
        }
        let usage_log = dir.join("state/itse/usage.jsonl");
        let usage_from = fs::metadata(&usage_log).map_or(0, |m| m.len());

        let o = self.o;
        let max_turns = o.max_turns.or(task.bench.max_turns).unwrap_or(40);
        let given = [("instruction", &task.instruction), ("model", &o.model), ("permission_mode", &o.permission_mode), ("task", &task.id), ("adapter_dir", &self.adapter_dir)];
        let dirs =
            [("ws", path(&ws)), ("cfg", path(&dir.join("cfg"))), ("trial", path(dir)), ("task_dir", path(&task.dir)), ("arm", arm.into()), ("max_turns", max_turns.to_string())];
        let arm_vars = self.adapter.arms[arm].iter().map(|(k, v)| (k.as_str(), v.clone()));
        let vars: BTreeMap<&str, String> = given.into_iter().map(|(k, v)| (k, v.clone())).chain(dirs).chain(arm_vars).collect();
        let argv = self.adapter.command.iter().map(|a| adapter::template(a, &vars)).collect::<Result<Vec<_>>>()?;
        let passed = self.adapter.pass_env.iter().filter_map(|key| Some((key.clone(), env::var(key).ok()?)));
        let set = self.adapter.env.iter().map(|(k, v)| Ok((k.clone(), adapter::template(v, &vars)?))).collect::<Result<Vec<_>>>()?;
        let harness_env: BTreeMap<String, String> = env.clone().into_iter().chain(passed).chain(set).collect();
        let started = Instant::now();
        let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
        let (exit, timed_out) = group(&mut command(&argv, &ws, &harness_env, &dir.join("harness.out"), Some(&dir.join("harness.err")))?, task.agent_timeout, true)?;
        let wall_ms = started.elapsed().as_millis() as u64;
        let line = serde_json::from_value(json!({
            "run": self.id, "trial": dir.file_name().map(|n| n.to_string_lossy()), "task": task.id, "task_sha": task.sha, "tags": task.tags, "arm": arm,
            "harness": o.harness, "harness_version": self.harness_version, "model": o.model, "itse_version": self.itse_version,
            "wall_ms": wall_ms, "exit": exit, "timed_out": timed_out, "max_turns": max_turns, "permission_mode": o.permission_mode, "agent_timeout_s": task.agent_timeout.as_secs_f64(),
            "itse": {"arm_clean": arm_clean, "used": if with { &task.bench.uses[..] } else { &[] }, "trusted_by": with.then_some("bench"), "skill": skill, "skill_home": with.then_some(home),
                "daemon": daemon(&dir.join("state/itse/ws")), "usage": usage(&usage_log, usage_from)},
        }))?;
        Ok((line, base))
    }

    /// `itse ARGS` in the trial's ws and env, appending to `itse-<verb>.log`; exit 0 or an error.
    fn itse(&self, env: &BTreeMap<String, String>, dir: &Path, args: &[&str]) -> Result<String> {
        let log = dir.join(format!("itse-{}.log", args[0]));
        let from = fs::metadata(&log).map_or(0, |m| m.len() as usize);
        let argv: Vec<&OsStr> = [self.itse.as_os_str()].into_iter().chain(args.iter().map(OsStr::new)).collect();
        let (exit, _) = group(&mut command(&argv, &dir.join("ws"), env, &log, None)?, Duration::from_secs(120), false)?;
        let text = fs::read_to_string(&log).unwrap_or_default().get(from..).unwrap_or_default().to_string();
        ensure!(exit == 0, "itse {} exited {exit}: {}", args.join(" "), text.trim());
        Ok(text)
    }
}

/// The bench's env as a trial sees it: the system's basics, proxy and CA settings and the toolchain's homes; no secrets, XDG homes or harness vars.
fn base_env(vars: impl Iterator<Item = (OsString, OsString)>) -> BTreeMap<String, String> {
    const KEEP: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "LANG", "TERM", "SHELL", "TMPDIR", "TZ", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_HOME"];
    const CA: &[&str] = &["SSL_CERT_FILE", "SSL_CERT_DIR", "CURL_CA_BUNDLE", "NODE_EXTRA_CA_CERTS", "NODE_USE_ENV_PROXY", "GIT_SSL_CAINFO", "REQUESTS_CA_BUNDLE"];
    let keep = |k: &str| KEEP.contains(&k) || CA.contains(&k) || k.starts_with("LC_") || k.to_ascii_lowercase().ends_with("_proxy");
    let mut env: BTreeMap<String, String> = vars.filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).filter(|(k, _)| keep(k)).collect();
    // A trial's HOME is its cfg/, so the toolchain's homes are named where the user's HOME has them.
    let home = PathBuf::from(env.get("HOME").cloned().unwrap_or_default());
    let homes = [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")].map(|(k, d)| (k.to_string(), home.join(d)));
    env.extend(homes.into_iter().filter(|(k, d)| home.is_absolute() && d.is_dir() && !env.contains_key(k)).map(|(k, d)| (k, d.display().to_string())).collect::<Vec<_>>());
    env
}

fn trial_env(base: &BTreeMap<String, String>, dir: &Path, with: bool) -> BTreeMap<String, String> {
    let mut env = base.clone();
    let homes = [("HOME", "cfg"), ("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "cfg"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "data/cache")];
    env.extend(homes.map(|(key, sub)| (key.to_string(), dir.join(sub).display().to_string())));
    env.insert("ITSE_SERVER_IDLE_S".into(), "60".into());
    // git reads no user or system config, the same on every machine: no signing, templates or hooks.
    env.extend([("GIT_CONFIG_NOSYSTEM", "1".into()), ("GIT_CONFIG_GLOBAL", dir.join("gitconfig").display().to_string())].map(|(k, v)| (k.into(), v)));
    let path = base.get("PATH").map_or("", String::as_str);
    with.then(|| env.insert("PATH".into(), format!("{}:{}:{path}", dir.join("bin").display(), dir.join("data/itse/bin").display())));
    env
}

/// `path` with each dir that offers itse (the binary, a copy named itse, or a shim linked to one) swapped for `tools/<n>`, which links its other
/// entries; masked dirs in a row share one, where the earlier wins, so every tool resolves as on `path`.
fn mask(path: &str, itse: &Path, tools: &Path) -> Result<String> {
    let (mut dirs, mut kept): (Vec<String>, usize) = (vec![], 0);
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let entries: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        let (masked, tools) = (entries.iter().any(|p| is_itse(p, itse)), tools.join(kept.to_string()));
        kept += usize::from(!masked);
        if masked {
            fs::create_dir_all(&tools)?;
            for entry in entries.iter().filter(|p| !is_itse(p, itse)) {
                let link = tools.join(entry.file_name().unwrap_or_default());
                symlink(entry, &link).or_else(|e| if e.kind() == ErrorKind::AlreadyExists { Ok(()) } else { Err(e) }).with_context(|| format!("linking {}", link.display()))?;
            }
        }
        dirs.push(if masked { tools.display().to_string() } else { dir.into() });
    }
    dirs.dedup();
    Ok(dirs.join(":"))
}

/// Whether `p` runs itse: the binary under test (by inode, so hard links count too) or any file that is, or links to, one named itse.
fn is_itse(p: &Path, itse: &Path) -> bool {
    let named = |p: &Path| p.file_name() == Some(OsStr::new("itse"));
    let same = fs::metadata(p).ok().zip(fs::metadata(itse).ok()).is_some_and(|(a, b)| (a.dev(), a.ino()) == (b.dev(), b.ino()));
    same || named(p) || fs::canonicalize(p).is_ok_and(|c| named(&c))
}

/// What `cmd` prints, trimmed, if it exits 0.
fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {cmd:?}"))?;
    ensure!(out.status.success(), "{cmd:?} exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// git in the agent's repo: the trial's env (no user config), its own group and a time limit, none of the repo's hooks or fsmonitor; stdout, trimmed.
fn git(env: &BTreeMap<String, String>, dir: &Path, args: &[&str]) -> Result<String> {
    let (out, err) = (dir.join("git.out"), dir.join("git.err"));
    fs::write(&out, "")?;
    let safe = ["git", "--no-pager", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"];
    let argv: Vec<&OsStr> = safe.iter().chain(args).map(OsStr::new).collect();
    let (exit, _) = group(&mut command(&argv, &dir.join("ws"), env, &out, Some(&err))?, Duration::from_secs(60), false)?;
    ensure!(exit == 0, "git {} exited {exit}; see {}", args.join(" "), err.display());
    Ok(fs::read_to_string(&out)?.trim().to_string())
}

/// What `sh -c SCRIPT` prints in `env`, if it succeeds.
fn sh(env: &BTreeMap<String, String>, cwd: &Path, script: &str) -> Option<String> {
    output(Command::new("sh").args(["-c", script]).env_clear().envs(env).current_dir(cwd)).ok()
}

/// `argv` in `cwd` with exactly `env`, stdout to `out` and stderr to `err` (else `out` too). Logs are appended to, so one log can hold several calls.
fn command(argv: &[&OsStr], cwd: &Path, env: &BTreeMap<String, String>, out: &Path, err: Option<&Path>) -> Result<Command> {
    let open = |p: &Path| OpenOptions::new().create(true).append(true).open(p).with_context(|| format!("opening {}", p.display()));
    let stdout = open(out)?;
    let stderr = if let Some(err) = err { open(err)? } else { stdout.try_clone()? };
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).env_clear().envs(env).stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    Ok(cmd)
}

/// Run `cmd` as the leader of its own process group, then end what is left of the group when the leader exits, `limit` passes or (if
/// `stoppable`) the run stops: SIGTERM, SIGKILL after 5 s. `(exit, timed_out)`; a signal's exit is 128 + its number, as in a shell.
pub fn group(cmd: &mut Command, limit: Duration, stoppable: bool) -> Result<(i32, bool)> {
    let mut child = cmd.process_group(0).spawn().with_context(|| format!("starting {:?}", cmd.get_program()))?;
    let (pgid, end) = (child.id() as i32, Instant::now() + limit);
    let (mut status, mut kill_at, mut timed_out) = (child.try_wait()?, None, false);
    while status.is_none() || !members(pgid).is_empty() {
        let now = Instant::now();
        if kill_at.is_none() && (status.is_some() || now >= end || stoppable && stopped()) {
            (timed_out, kill_at) = (status.is_none(), Some(now + Duration::from_secs(5)));
            signal(-pgid, "TERM");
        } else if kill_at.is_some_and(|t| now >= t) {
            signal(-pgid, "KILL");
            break;
        }
        thread::sleep(Duration::from_millis(20));
        status = status.or(child.try_wait()?);
    }
    let status = if let Some(s) = status { s } else { child.wait()? };
    Ok((status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)), timed_out))
}

/// `kill -s SIG -- PID`; a negative pid is a group.
fn signal(pid: i32, sig: &str) {
    let _ = Command::new("kill").args(["-s", sig, "--", &pid.to_string()]).stderr(Stdio::null()).status();
}

fn pids() -> impl Iterator<Item = i32> {
    fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok())
}

/// The live (not zombie) processes of group `pgid`.
fn members(pgid: i32) -> Vec<i32> {
    let stat = |pid: i32| -> Option<(char, i32)> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let mut fields = text[text.rfind(')')? + 1..].split_whitespace();
        Some((fields.next()?.chars().next()?, fields.nth(1)?.parse().ok()?))
    };
    pids().filter(|&pid| stat(pid).is_some_and(|(state, group)| group == pgid && !matches!(state, 'Z' | 'X'))).collect()
}

/// Processes still carrying the trial's own `XDG_STATE_HOME` 5 s on (not ones that merely name its dir, as `tail -f` does), killed and recorded.
fn reap(dir: &Path) -> Vec<i32> {
    let marker = format!("XDG_STATE_HOME={}", dir.join("state").display()).into_bytes();
    let names = |pid: i32| fs::read(format!("/proc/{pid}/environ")).is_ok_and(|b| b.split(|&c| c == 0).any(|e| e == marker));
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let left: Vec<i32> = pids().filter(|&pid| pid != process::id() as i32 && names(pid)).collect();
        if left.is_empty() || Instant::now() >= end {
            left.iter().for_each(|pid| signal(*pid, "KILL"));
            return left;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// The trial's daemon, from `daemon.json` while it runs: pid, version and VmHWM.
fn daemon(ws_dirs: &Path) -> Value {
    let read = |p: PathBuf| fs::read(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let Some(info) = fs::read_dir(ws_dirs).into_iter().flatten().flatten().find_map(|d| read(d.path().join("daemon.json"))) else { return Value::Null };
    let status = fs::read_to_string(format!("/proc/{}/status", info["pid"])).unwrap_or_default();
    let hwm = status.lines().find_map(|l| l.strip_prefix("VmHWM:")).and_then(|v| v.trim().trim_end_matches(" kB").parse::<u64>().ok());
    json!({"pid": info["pid"], "version": info["version"], "vmhwm_kb": hwm})
}

/// The trial's own usage log from byte `from`: calls by verb, errors, bytes, and the MCP tool list's bytes.
fn usage(log: &Path, from: u64) -> Value {
    let bytes = fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes[(from as usize).min(bytes.len())..]).into_owned();
    let recs: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let of = |ev: &str| recs.iter().filter(|r| r["ev"] == ev).collect::<Vec<_>>();
    let (calls, inits) = (of("call"), of("init"));
    let sum = |rs: &[&Value], key: &str| rs.iter().map(|r| r[key].as_u64().unwrap_or(0)).sum::<u64>();
    let mut by_verb = BTreeMap::<String, u64>::new();
    calls.iter().for_each(|r| *by_verb.entry(r["verb"].as_str().unwrap_or("unknown").into()).or_default() += 1);
    let errors = calls.iter().filter(|r| r["outcome"] == "error").count();
    json!({"calls": calls.len(), "by_verb": by_verb, "errors": errors, "bytes": sum(&calls, "bytes"), "list_bytes": sum(&inits, "list_bytes")})
}

/// xorshift64 (Marsaglia 2003): the arms' order per rep, reproducible from the seed.
pub fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

#[cfg(test)]
#[path = "../tests/unit/run.rs"]
mod tests;
