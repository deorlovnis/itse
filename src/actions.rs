//! Long actions: VS Code task entries expanded into steps, run by a re-exec'd runner
//! so a run outlives the daemon and its exit status is never lost, then waited on (to the end or to
//! a `ready` line), cancelled with one escalation, and re-adopted by the next daemon.
//!
//! One `run.json` per run instead of spec, pid and exit files: the daemon writes the spec, the
//! runner adds its pid and then the exit, each time atomically. The env is never written: it goes
//! on the runner's `Command` only.

use std::os::unix::{fs::OpenOptionsExt, process::ExitStatusExt};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError, atomic::AtomicBool, atomic::Ordering::SeqCst};
use std::{
    collections::BTreeMap, env, fs, fs::File, fs::OpenOptions, io::Read, io::Write, path::Path, path::PathBuf, thread, time::Duration, time::Instant, time::SystemTime,
    time::UNIX_EPOCH,
};

use anyhow::{Result, anyhow, ensure};
use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::results::{Cost, Outcome, Store, Verdict};
use crate::{host, host::Host, host::Limits, manifest::Action, manifest::OneOrMany, manifest::render, resolve::Resolved, rpc::Cancel, sys, sys::lock};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunRecord {
    pub spec: RunSpec,
    pub pid: Option<RunPid>,
    pub exit: Option<RunExit>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunSpec {
    /// `(argv, cwd)` of the `dependsOn` steps in order, then the action's own; the first that fails
    /// ends the run.
    pub steps: Vec<(Vec<String>, PathBuf)>,
    pub limits: Limits,
    pub label: String,
    pub started_at: String,
    /// The start in ms since the epoch: reports older than it are stale.
    pub started_ms: u64,
    /// When the action's timeout ends the run, in ms since the epoch, so a re-adopting daemon keeps it.
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunPid {
    pub runner_pid: i32,
    pub child_pid: i32,
    /// The runner's start time, so a reused pid isn't taken for the run.
    pub start_time: u64,
}

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub ended_at: String,
    pub ms: u64,
}

impl RunRecord {
    pub fn load(dir: &Path) -> Option<Self> {
        fs::read(dir.join("run.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn save(&self, dir: &Path) -> Result<()> {
        sys::write_atomic(&dir.join("run.json"), &serde_json::to_vec(self)?)
    }
}

/// A run that hasn't ended, as the daemon tracks it. Its runner leads the run's process group.
#[derive(Default)]
pub struct Run {
    pub handle: String,
    pub dir: PathBuf,
    pub pgid: i32,
    start_time: u64,
    /// Set by the first cancel or timeout, so the escalation runs once.
    stopping: AtomicBool,
    timed_out: AtomicBool,
    done: Mutex<bool>,
    ended: Condvar,
}

impl Run {
    /// Whether the run has ended, waiting up to `wait` for it.
    pub fn ended(&self, wait: Duration) -> bool {
        *self.ended.wait_timeout_while(lock(&self.done), wait, |done| !*done).unwrap_or_else(PoisonError::into_inner).0
    }
}

/// What a label runs: its steps, the env its entries add, and the action's own timeout and ready
/// pattern; `code` if any step runs project code.
#[derive(Debug, Default)]
pub struct Plan {
    pub steps: Vec<(Vec<String>, PathBuf)>,
    pub env: BTreeMap<String, String>,
    pub timeout_s: Option<u64>,
    pub ready: Option<Regex>,
    pub code: bool,
}

/// Expand `label` and its `dependsOn` (depth first, in order) into steps. Templates are filled from
/// the resolved packages and `inputs`; an arg that is just `${target_args}` or `${filter_args}`
/// becomes that list, or nothing when an input in it is empty. `extra` goes to `label` itself. A
/// `shell` task runs `/bin/sh -c` on its command and its args, each in single quotes.
pub fn plan(r: &Resolved, root: &Path, cache: &Path, label: &str, inputs: &BTreeMap<String, String>, extra: &[String]) -> Result<Plan> {
    let declared: Vec<&Action> = r.manifest.uses().iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| &e.0.actions).collect();
    let (mut order, mut plan) = (Vec::new(), Plan::default());
    expand(&declared, label, &mut Vec::new(), &mut order)?;
    let get = |key: &str| match key.split_once(':') {
        Some(("pkg", p)) => r.roots.get(p).map(|d| d.display().to_string()),
        Some(("input", i)) => inputs.get(i).cloned(),
        _ => [("workspaceFolder", root), ("cache", cache)].iter().find(|(k, _)| *k == key).map(|(_, p)| p.display().to_string()),
    };
    let one = |s: &str| render(s, &mut |k| get(k)).map_err(|e| e.context(format!("{label} takes --input name=value")));
    let list = |xs: &[String]| xs.iter().map(|x| render(x, &mut |k| get(k).filter(|v| !v.is_empty()))).collect::<Result<Vec<_>>>().unwrap_or_default();
    for a in order {
        let mut args = vec![one(&a.command)?];
        let lists = [("${target_args}", &a.target_args), ("${filter_args}", &a.filter_args)];
        for arg in &a.args {
            if let Some((_, l)) = lists.iter().find(|(k, _)| k == arg) { args.extend(list(l)) } else { args.push(one(arg)?) }
        }
        args.extend(extra.iter().filter(|_| a.label == label).cloned());
        if a.kind == "shell" {
            let quoted = args[1..].iter().map(|a| format!("'{}'", a.replace('\'', r"'\''")));
            args = vec!["/bin/sh".into(), "-c".into(), [args[0].clone()].into_iter().chain(quoted).collect::<Vec<_>>().join(" ")];
        }
        let cwd = a.options.cwd.as_ref().map(|c| one(c)).transpose()?.map_or(root.to_path_buf(), |c| root.join(c));
        plan.env.extend(a.options.env.iter().map(|(k, v)| Ok((k.clone(), one(v)?))).collect::<Result<Vec<_>>>()?);
        plan.steps.extend((!a.command.is_empty()).then_some((args, cwd)));
        (plan.code, plan.timeout_s, plan.ready) = (plan.code || a.executes_project_code, a.timeout_s, a.ready.as_deref().map(Regex::new).transpose()?);
    }
    Ok(plan)
}

fn expand<'a>(declared: &[&'a Action], label: &str, path: &mut Vec<String>, out: &mut Vec<&'a Action>) -> Result<()> {
    ensure!(!path.iter().any(|l| l == label), "dependsOn cycle: {} → {label}", path.join(" → "));
    let labels = || declared.iter().map(|a| a.label.as_str()).collect::<Vec<_>>().join(", ");
    let a = declared.iter().find(|a| a.label == label).ok_or_else(|| anyhow!("no action '{label}' in the extensions in use; declared: {}", labels()))?;
    path.push(label.into());
    a.depends_on.iter().flat_map(OneOrMany::as_slice).try_for_each(|d| expand(declared, d, path, out))?;
    path.pop();
    out.push(a);
    Ok(())
}

/// Write `run.json`, start `/proc/self/exe __run <dir>` in its own session with `env` (which goes
/// on the `Command` only), add it to `procs.json`, and watch it on a thread.
pub fn start(host: &Arc<Host>, label: &str, plan: &Plan, env: &BTreeMap<String, String>) -> Result<Arc<Run>> {
    let (handle, started_at) = (host::Next::take(&host.ws.state_dir(&host.dirs), "run")?, sys::utc(SystemTime::now()));
    let dir = host.ws.state_dir(&host.dirs).join("runs").join(&handle[4..]);
    sys::private_dir(&dir)?;
    let started_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let deadline_ms = plan.timeout_s.map(|s| started_ms + s * 1000);
    let spec = RunSpec { steps: plan.steps.clone(), limits: Limits::default(), label: label.into(), started_at: started_at.clone(), started_ms, deadline_ms };
    RunRecord { spec, ..RunRecord::default() }.save(&dir)?;
    let (mut cmd, log) = (Command::new("/proc/self/exe"), OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("out.log"))?);
    cmd.arg("__run").arg(&dir).env_clear().envs(env).current_dir(&dir).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    let child = host::spawn(&mut cmd, &Limits::default(), true, 500).map_err(|e| anyhow!("starting the runner: {e}"))?;
    let (pid, start_time) = (child.id() as i32, sys::proc_stat(child.id() as i32).map_or(0, |s| s.start_time));
    let record = json!({"handle": handle, "kind": "run", "pid": pid, "pgid": pid, "start_time": start_time, "argv0": label, "started_at": started_at, "log": dir.join("out.log")});
    let run = Arc::new(Run { handle, dir, pgid: pid, start_time, ..Run::default() });
    watch(host, &run, record, Some(child));
    Ok(run)
}

/// `itse __run <dir>`: the runner, leader of the run's group. It runs the steps with stdout and
/// stderr on one fd appended to `out.log`, records its pid, then the exit. SIGTERM, SIGINT and
/// SIGHUP only set a flag here, so the exit is written whatever the group gets short of SIGKILL, and
/// a cancel that comes between steps (or before the first) starts no other.
pub fn runner(dir: &Path) -> Result<u8> {
    sys::catch_signals();
    // Written here too, before any step starts, so the steps inherit it whatever the daemon's timing.
    let _ = fs::write("/proc/self/oom_score_adj", "500");
    let mut rec = RunRecord::load(dir).ok_or_else(|| anyhow!("no run.json in {}", dir.display()))?;
    let mut log = OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("out.log"))?;
    let (started, mut exit, me, start_time) = (Instant::now(), RunExit::default(), process::id() as i32, sys::proc_stat(process::id() as i32).map_or(0, |s| s.start_time));
    for (argv, cwd) in rec.spec.steps.clone() {
        // The first step that fails or is cancelled ends the run.
        exit.cancelled |= sys::signalled();
        let Some((program, args)) = argv.split_first().filter(|_| !exit.cancelled && exit.signal.is_none() && exit.code.unwrap_or(0) == 0) else { break };
        let spawned = Command::new(program).args(args).current_dir(cwd).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?).spawn();
        exit.code = spawned.is_err().then_some(127);
        let Ok(mut child) = spawned.inspect_err(|e| drop(writeln!(log, "itse: starting {program}: {e}"))) else { break };
        rec.pid = Some(RunPid { runner_pid: me, child_pid: child.id() as i32, start_time });
        // Without its pid on record the step still runs: a cancel then reaches it through the group.
        let _ = rec.save(dir).inspect_err(|e| drop(writeln!(log, "itse: {e:#}")));
        let status = child.wait()?;
        (exit.code, exit.signal, exit.cancelled) = (status.code(), status.signal(), exit.cancelled || sys::signalled());
    }
    rec.exit = Some(RunExit { ended_at: sys::utc(SystemTime::now()), ms: started.elapsed().as_millis() as u64, ..exit });
    rec.save(dir).map(|()| 0)
}

/// Track a run in `procs.json` and the registry, and wait for its end on a thread: the runner is
/// reaped when it is our child, else (re-adopted) polled every second for its exit record and its
/// liveness. The deadline in `run.json` starts the escalation once, in this daemon or the next.
fn watch(host: &Arc<Host>, run: &Arc<Run>, record: Value, mut child: Option<Child>) {
    host.track(&run.handle, Some(record));
    lock(&host.runs).insert(run.handle.clone(), run.clone());
    let (host, run) = (host.clone(), run.clone());
    thread::spawn(move || {
        let adopted = || sys::alive(run.pgid, run.start_time) && RunRecord::load(&run.dir).is_some_and(|r| r.exit.is_none());
        let (deadline, mut status) = (RunRecord::load(&run.dir).and_then(|r| r.spec.deadline_ms).map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)), None);
        while child.as_mut().map_or_else(adopted, |c| c.try_wait().map(|s| status = s).is_ok() && status.is_none()) {
            if deadline.is_some_and(|d| SystemTime::now() >= d) && !run.stopping.load(SeqCst) && !run.timed_out.swap(true, SeqCst) {
                let run = run.clone();
                thread::spawn(move || stop_run(&run));
            }
            thread::sleep(Duration::from_millis(if child.is_some() { 100 } else { 1000 }));
        }
        finish(&host, &run, status);
    });
}

/// Record how a run ended: a runner that ended before it wrote its exit gets its own status when it
/// was our child, else (re-adopted) `{signal: 9, cancelled}`; our cancel or timeout is marked. Then
/// the log and reports are parsed and the result stored, before the waiters are woken: so a
/// run that outlasts every wait, or ends under the next daemon, is parsed too, and an ended run
/// without its `result.json` is one still settling.
fn finish(host: &Host, run: &Run, status: Option<ExitStatus>) {
    if let Some(mut rec) = RunRecord::load(&run.dir) {
        if rec.exit.is_none() || run.stopping.load(SeqCst) {
            let (code, signal) = status.map_or((None, Some(sys::SIGKILL)), |s| (s.code(), s.signal()));
            let exit = rec.exit.get_or_insert_with(|| RunExit { code, signal, cancelled: status.is_none(), ended_at: sys::utc(SystemTime::now()), ..RunExit::default() });
            (exit.cancelled, exit.timed_out) = (exit.cancelled || run.stopping.load(SeqCst), run.timed_out.load(SeqCst));
            let _ = rec.save(&run.dir).inspect_err(|e| eprintln!("itse daemon: {}: {e:#}", run.handle));
        }
        let mut o = outcome("run", &run.handle, &run.dir);
        crate::parse::ended(&host.ws, &host.dirs, &rec, &run.dir, &mut o);
        let _ = Store(host.ws.state_dir(&host.dirs)).save(&o, "").inspect_err(|e| eprintln!("itse daemon: results: {e:#}"));
    }
    lock(&host.runs).remove(&run.handle);
    host.track(&run.handle, None);
    *lock(&run.done) = true;
    run.ended.notify_all();
}

/// Cancel and timeout: SIGINT to the action, after the grace SIGTERM to the group, after
/// another grace SIGKILL; each step only while the group has members and its leader is still ours.
/// Then wait for the end to be recorded. The grace is 5 s; debug builds take `ITSE_TEST_GRACE_MS`.
/// A pid on record goes only to a step still in the run's group, and no pgid below 2 is signalled.
pub fn stop_run(run: &Run) {
    let child = RunRecord::load(&run.dir).and_then(|r| r.pid).map(|p| p.child_pid).filter(|&c| c > 1 && sys::proc_stat(c).is_some_and(|s| s.pgid == run.pgid)).unwrap_or(run.pgid);
    let live = || sys::send_signal(-run.pgid, 0) && sys::proc_stat(run.pgid).is_none_or(|s| s.start_time == run.start_time);
    let (steps, first) = ([(child, sys::SIGINT), (-run.pgid, sys::SIGTERM), (-run.pgid, sys::SIGKILL)], !run.stopping.swap(true, SeqCst));
    for (pid, signal) in steps.into_iter().filter(|_| first && run.pgid > 1).take_while(|_| live()) {
        sys::send_signal(pid, signal);
        let grace = env::var("ITSE_TEST_GRACE_MS").ok().filter(|_| cfg!(debug_assertions)).and_then(|ms| ms.parse().ok()).unwrap_or(5000);
        sys::wait_until(Duration::from_millis(grace), || !live());
    }
    run.ended(Duration::from_secs(2));
}

/// Wait up to `limit` for the run's end or a cancel, or, given a pattern, for its first matching
/// line of output, whose capture groups it returns (`${ready:1}` first). Each look reads only what
/// was appended since the last, in whole lines, and one more look follows the end.
pub fn wait(run: &Run, ready: Option<&Regex>, limit: Duration, cancel: &Cancel) -> Option<Vec<String>> {
    let (end, mut log, mut buf, mut over) = (Instant::now() + limit, File::open(run.dir.join("out.log")).ok(), Vec::new(), false);
    while !over {
        over = run.ended(Duration::from_millis(50)) || cancel.is_cancelled() || Instant::now() >= end;
        let _ = log.as_mut().filter(|_| ready.is_some()).map(|f| f.read_to_end(&mut buf));
        let whole: Vec<u8> = buf.drain(..buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)).collect();
        let text = String::from_utf8_lossy(&whole);
        if let Some(c) = ready.and_then(|re| text.lines().find_map(|l| re.captures(l))) {
            return Some(c.iter().skip(1).map(|m| m.map_or("", |m| m.as_str()).to_string()).collect());
        }
    }
    None
}

/// A run's result: RUNNING with the time so far and its last line, else how it ended; then the end
/// of its output and where the whole log is.
pub fn outcome(verb: &str, handle: &str, dir: &Path) -> Outcome {
    let (rec, log, tail) = (RunRecord::load(dir).unwrap_or_default(), dir.join("out.log"), host::tail(&dir.join("out.log"), 10));
    let age = fs::metadata(dir).and_then(|m| m.created().or(m.modified())).ok().and_then(|t| t.elapsed().ok()).unwrap_or_default().as_secs();
    let (verdict, counts, err) = match &rec.exit {
        None => (Verdict::Running, format!("{age} s so far; last line {:?}", tail.last().map_or("", String::as_str)), None),
        Some(e) if e.timed_out => (Verdict::Fail, format!("timed out after {:.1} s", e.ms as f64 / 1000.0), Some("timeout")),
        Some(e) if e.cancelled => (Verdict::Stopped, format!("cancelled{}", e.signal.map_or(String::new(), |s| format!(" (signal {s})"))), None),
        Some(e) if e.code == Some(0) => (Verdict::Ok, "exit 0".into(), None),
        Some(e) => (Verdict::Fail, e.code.map_or_else(|| format!("killed by signal {}", e.signal.unwrap_or(0)), |c| format!("exit {c}")), None),
    };
    let (ms, notes) = (rec.exit.map_or(0, |e| e.ms), tail.into_iter().chain([format!("log: {}", log.display())]).collect());
    Outcome { id: Some(handle.into()), cost: Cost { ms, warm: None }, notes, err, ..Outcome::new(verb, verdict, rec.spec.label, counts) }
}

/// Re-adopt a run from the old `procs.json`: watched again while its runner lives (its start time,
/// not a zombie) and hasn't written its exit; otherwise its end is recorded now.
pub fn adopt(host: &Arc<Host>, p: Value) {
    let handle = p["handle"].as_str().unwrap_or_default();
    let Some(n) = handle.strip_prefix("run:").filter(|n| n.parse::<u64>().is_ok()) else { return };
    let (pid, start) = (p["pid"].as_i64().unwrap_or_default() as i32, p["start_time"].as_u64().unwrap_or_default());
    let run = Arc::new(Run { handle: handle.into(), dir: host.ws.state_dir(&host.dirs).join("runs").join(n), pgid: pid, start_time: start, ..Run::default() });
    if pid > 1 && sys::alive(pid, start) && RunRecord::load(&run.dir).is_some_and(|r| r.exit.is_none()) { watch(host, &run, p, None) } else { finish(host, &run, None) }
}

#[cfg(test)]
mod tests;
