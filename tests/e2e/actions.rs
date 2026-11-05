//! Actions end to end: declared actions run through the re-exec'd `__run` runner, their
//! `run.json`, timeouts and cancels with the SIGINT → SIGTERM → SIGKILL escalation, the `ready`
//! capture, and runs that outlive a killed daemon or a replaced binary. The actions are `/bin/sh`
//! scripts in an added extension `jobs`, so no fixture server is needed; the escalation's grace is
//! cut to 300 ms through the debug-only `ITSE_TEST_GRACE_MS`.

use std::os::unix::process::{CommandExt, ExitStatusExt};

use super::*;

const GRACE: &str = "300";

/// A workspace that uses the extension `jobs` with these actions; every run left is killed on drop.
struct Jobs {
    env: Env,
}

impl Jobs {
    fn new(actions: &str) -> Self {
        let env = Env::new();
        fs::create_dir_all(env.work().join(".git")).unwrap();
        let file = env.dir.join("jobs.json");
        fs::write(&file, format!(r#"{{"itse_ext": 1, "id": "jobs", "actions": {actions}}}"#)).unwrap();
        let jobs = Self { env };
        for args in [vec!["ext", "add", file.to_str().unwrap()], vec!["use", "jobs"]] {
            let (code, text) = jobs.itse(&args);
            assert_eq!(code, Some(0), "{args:?}: {text}");
        }
        jobs
    }

    fn cmd(&self, exe: &str, args: &[&str]) -> Command {
        let mut cmd = self.env.command_of(exe, &self.env.work(), args);
        cmd.env("ITSE_TEST_GRACE_MS", GRACE);
        cmd
    }

    /// `itse ARGS` in the work folder: (exit code, stdout and stderr).
    fn itse(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.cmd(ITSE, args).output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    }

    fn run_dir(&self, n: u32) -> PathBuf {
        self.env.daemons().first().expect("a daemon ran").0.join(format!("runs/{n}"))
    }

    fn run_json(&self, n: u32) -> Value {
        fs::read(self.run_dir(n).join("run.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
    }

    /// The runner's pid of run `n`, the leader of its group, once it has started a step.
    fn runner(&self, n: u32) -> i32 {
        assert!(wait_until(Duration::from_secs(5), || self.run_json(n)["pid"].is_object()), "run {n} never started: {}", self.run_json(n));
        self.run_json(n)["pid"]["runner_pid"].as_i64().unwrap() as i32
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        let Some((ws, _)) = self.env.daemons().into_iter().next() else { return };
        for run in fs::read_dir(ws.join("runs")).into_iter().flatten().flatten() {
            let pid = fs::read(run.path().join("run.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["pid"]["runner_pid"].as_i64());
            if let Some(pid) = pid.filter(|&p| p > 1) {
                signal(-(pid as i32), "KILL");
            }
        }
    }
}

/// Whether any process is still in process group `pgid`.
fn group_alive(pgid: i32) -> bool {
    let pids = fs::read_dir("/proc").unwrap().flatten().filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok());
    pids.into_iter().any(|pid| stat_field(pid, 5) == Some(pgid.to_string()) && alive(pid))
}

const SCRIPTS: &str = r#"[
    {"label": "jobs:hello", "type": "process", "command": "/bin/sh", "options": {"env": {"JOB_ROOT": "${workspaceFolder}"}},
     "args": ["-c", "echo hello; echo oom $(cat /proc/self/oom_score_adj); echo pgid $(cut -d' ' -f5 /proc/$$/stat); echo root=$JOB_ROOT caller=$JOB_CALLER; echo args \"$@\"", "sh"]},
    {"label": "jobs:fail", "type": "shell", "command": "echo half way; exit 3"},
    {"label": "jobs:both", "type": "shell", "command": "echo second", "dependsOn": ["jobs:hello"]},
    {"label": "jobs:slow", "command": "/bin/sh", "args": ["-c", "sleep 30 & sleep 30; wait"], "timeout_s": 1},
    {"label": "jobs:stubborn", "command": "/bin/sh", "ready": "^ready$",
     "args": ["-c", "trap 'echo got INT' INT; trap 'echo got TERM' TERM; echo ready; while :; do sleep 0.05; done"]},
    {"label": "jobs:serve", "type": "shell", "command": "echo starting; sleep 0.3; echo listening on 127.0.0.1:4711; sleep 30", "ready": "listening on [0-9.]+:(\\d+)"},
    {"label": "jobs:sleeper", "command": "sleep", "args": ["30"]},
    {"label": "jobs:later", "type": "shell", "command": "sleep 1.5; echo done"},
    {"label": "jobs:code", "type": "shell", "command": "echo built", "executes_project_code": true},
    {"label": "jobs:capped", "command": "sleep", "args": ["30"], "timeout_s": 2}
]"#;

#[test]
fn a_run_answers_with_its_exit_and_the_end_of_its_output() {
    let jobs = Jobs::new(SCRIPTS);
    let out = jobs.cmd(ITSE, &["run", "jobs:hello", "--", "a b"]).env("JOB_CALLER", "the-cli").output().unwrap();
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.starts_with("OK run jobs:hello — exit 0; run:1\nhello\noom 500\n"), "the runner and its steps get oom_score_adj 500: {text}");
    let work = jobs.env.work();
    assert!(text.contains(&format!("root={} caller=the-cli\nargs a b\n", work.display())), "templates, the caller's env and the args after --: {text}");
    let run = jobs.run_json(1);
    let (runner, child) = (run["pid"]["runner_pid"].as_i64().unwrap(), run["pid"]["child_pid"].as_i64().unwrap());
    assert!(text.contains(&format!("pgid {runner}\n")) && runner != child, "the runner leads the step's group: {text}");
    assert_eq!(stat_field(jobs.env.daemon_json()["pid"].as_i64().unwrap() as i32, 6).map(|s| s != runner.to_string()), Some(true));
    assert_eq!((run["exit"]["code"].as_i64(), run["exit"]["cancelled"].as_bool()), (Some(0), Some(false)));
    assert_eq!(run["spec"]["steps"][0][1], json!(work), "the step runs in the workspace");
    assert!(!fs::read_to_string(jobs.run_dir(1).join("run.json")).unwrap().contains("the-cli"), "the env is never written");
    let (code, shown) = jobs.itse(&["show", "run:1"]);
    assert_eq!((code, shown.lines().next()), (Some(0), Some("OK run jobs:hello — exit 0; run:1")), "{shown}");
    let (code, text) = jobs.itse(&["run", "jobs:fail"]);
    assert_eq!((code, text.lines().take(2).collect::<Vec<_>>()), (Some(1), vec!["FAIL run jobs:fail — exit 3; run:2", "half way"]));
    let (code, text) = jobs.itse(&["run", "jobs:both"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("hello\n") && text.contains("args\nsecond\n"), "dependsOn runs first: {text}");
    let (code, text) = jobs.itse(&["run", "jobs:nope"]);
    assert_eq!(code, Some(2));
    assert!(text.starts_with("ERROR run — no action 'jobs:nope' in the extensions in use; declared: jobs:hello, jobs:fail, jobs:both, jobs:slow,"), "{text}");
}

#[test]
fn a_timed_out_action_kills_its_process_group() {
    let jobs = Jobs::new(SCRIPTS);
    let started = Instant::now();
    let (code, text) = jobs.itse(&["run", "jobs:slow"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.starts_with("FAIL run jobs:slow — timed out after 1."), "{text}");
    let pgid = jobs.runner(1);
    assert!(wait_until(Duration::from_secs(3), || !group_alive(pgid)), "the background sleeps go with the group");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    let exit = jobs.run_json(1)["exit"].clone();
    assert_eq!((exit["timed_out"].as_bool(), exit["cancelled"].as_bool()), (Some(true), Some(true)), "{exit}");
}

#[test]
fn cancel_escalates_from_sigint_to_sigterm_to_sigkill() {
    let jobs = Jobs::new(SCRIPTS);
    let (code, text) = jobs.itse(&["run", "jobs:stubborn"]);
    assert_eq!((code, text.lines().next()), (Some(4), Some("RUNNING run jobs:stubborn — ready; run:1")), "{text}");
    let (pgid, started) = (jobs.runner(1), Instant::now());
    let (code, text) = jobs.itse(&["cancel", "run:1"]);
    assert_eq!((code, text.lines().next()), (Some(0), Some("STOPPED cancel jobs:stubborn — cancelled (signal 9); run:1")), "{text}");
    assert!(started.elapsed() >= Duration::from_millis(600), "two graces of {GRACE} ms: {:?}", started.elapsed());
    let log = fs::read_to_string(jobs.run_dir(1).join("out.log")).unwrap();
    assert_eq!(log.lines().filter(|l| l.starts_with("got")).collect::<Vec<_>>(), ["got INT", "got TERM"], "{log}");
    assert!(!group_alive(pgid));
    let exit = jobs.run_json(1)["exit"].clone();
    assert_eq!((exit["signal"].as_i64(), exit["cancelled"].as_bool()), (Some(9), Some(true)), "the daemon writes the exit of a SIGKILLed runner");
    let (code, text) = jobs.itse(&["cancel", "run:1"]);
    assert_eq!((code, text.lines().next()), (Some(0), Some("STOPPED cancel jobs:stubborn — cancelled (signal 9); run:1")), "a second cancel shows the end");
    assert_eq!(jobs.itse(&["cancel", "run:9"]), (Some(2), "ERROR cancel — no run run:9\n".into()));
}

#[test]
fn the_ready_pattern_returns_its_capture_while_the_run_goes_on() {
    let jobs = Jobs::new(SCRIPTS);
    let started = Instant::now();
    let (code, text) = jobs.itse(&["run", "jobs:serve"]);
    assert_eq!((code, text.lines().next()), (Some(4), Some("RUNNING run jobs:serve — ready ${ready:1}=4711; run:1")), "{text}");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(group_alive(jobs.runner(1)), "the run goes on after its ready line");
    let (code, text) = jobs.itse(&["run", "jobs:serve"]);
    assert_eq!(code, Some(4), "{text}");
    assert!(text.contains("\nrun:1 of jobs:serve is running too\n"), "runs of one label may overlap, and say so: {text}");
    assert_eq!(jobs.itse(&["cancel", "run:2"]).0, Some(0));
    let (code, text) = jobs.itse(&["show", "run:1"]);
    assert_eq!(code, Some(0));
    assert!(text.starts_with("RUNNING run jobs:serve — ") && text.contains("listening on 127.0.0.1:4711"), "{text}");
    assert_eq!(jobs.itse(&["cancel", "run:1"]).0, Some(0));
}

#[test]
fn cancel_kills_the_run_process_group() {
    let jobs = Jobs::new(SCRIPTS);
    let mut mcp = jobs.env.mcp(&[("ITSE_TEST_GRACE_MS", GRACE)]);
    mcp.handshake("e2e");
    let (is_error, text) = mcp.call("test_build_run", json!({"verb": "run", "args": ["jobs:sleeper"], "wait": 1}));
    assert!(!is_error && text.starts_with("RUNNING run jobs:sleeper — ") && text.lines().next().unwrap().ends_with("; run:1"), "{text}");
    let pgid = jobs.runner(1);
    assert!(group_alive(pgid));
    let (is_error, text) = mcp.call("test_build_run", json!({"verb": "cancel", "target": "run:1"}));
    assert!(!is_error && text.starts_with("STOPPED cancel jobs:sleeper — cancelled"), "{text}");
    assert!(!group_alive(pgid), "the group is gone when cancel answers");
}

#[test]
fn ctrl_c_in_the_cli_cancels_its_run_and_a_closed_client_leaves_it_running() {
    let jobs = Jobs::new(SCRIPTS);
    let cli = jobs.cmd(ITSE, &["run", "jobs:sleeper"]).stdout(Stdio::piped()).spawn().unwrap();
    let pgid = jobs.runner(1);
    signal(cli.id() as i32, "INT");
    let out = cli.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stdout(&out));
    assert!(stdout(&out).starts_with("STOPPED run jobs:sleeper — cancelled"), "{}", stdout(&out));
    assert!(!group_alive(pgid));
    let mut cli = jobs.cmd(ITSE, &["run", "jobs:sleeper"]).spawn().unwrap();
    let pgid = jobs.runner(2);
    cli.kill().unwrap();
    cli.wait().unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(group_alive(pgid), "a run is a handle: its caller going away detaches it");
    assert_eq!(jobs.itse(&["cancel", "run:2"]).0, Some(0));
}

#[test]
fn killing_the_daemon_keeps_results_showable() {
    let jobs = Jobs::new(SCRIPTS);
    let cli = jobs.cmd(ITSE, &["run", "jobs:later"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let runner = jobs.runner(1);
    let daemon = jobs.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    signal(daemon, "KILL");
    assert!(wait_until(Duration::from_secs(2), || !alive(daemon)));
    drop(cli.wait_with_output());
    // Two processes the old daemon "left": one of ours, one whose pid now has another start time.
    let sleeper = || Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let (mut ours, mut reused) = (sleeper(), sleeper());
    let start = |c: &Child| stat_field(c.id() as i32, 22).unwrap().parse::<u64>().unwrap();
    let procs_path = jobs.run_dir(1).parent().unwrap().parent().unwrap().join("procs.json");
    let mut procs: Vec<Value> = serde_json::from_slice(&fs::read(&procs_path).unwrap()).unwrap();
    assert_eq!(procs.iter().map(|p| (p["handle"].as_str(), p["pid"].as_i64())).collect::<Vec<_>>(), [(Some("run:1"), Some(runner.into()))]);
    procs.push(json!({"handle": "lsp:left", "kind": "lsp", "pid": ours.id(), "pgid": ours.id(), "start_time": start(&ours)}));
    procs.push(json!({"handle": "lsp:other", "kind": "lsp", "pid": reused.id(), "pgid": reused.id(), "start_time": start(&reused) + 1}));
    fs::write(&procs_path, serde_json::to_vec(&procs).unwrap()).unwrap();
    assert!(wait_until(Duration::from_secs(5), || jobs.run_json(1)["exit"].is_object()), "the runner writes its exit without a daemon");
    let (code, text) = jobs.itse(&["show", "run:1"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK run jobs:later — exit 0; ") && text.contains("\ndone\n"), "{text}");
    assert_eq!(ours.wait().unwrap().signal(), Some(15), "a leftover of the old daemon is killed");
    assert_eq!(reused.try_wait().unwrap(), None, "a pid with the wrong start time is ignored");
    reused.kill().unwrap();
    reused.wait().unwrap();
}

#[test]
fn a_run_still_going_when_the_daemon_dies_is_re_adopted() {
    let jobs = Jobs::new(SCRIPTS);
    let (code, text) = jobs.itse(&["run", "jobs:serve"]);
    assert_eq!(code, Some(4), "{text}");
    let pgid = jobs.runner(1);
    let daemon = jobs.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    signal(daemon, "KILL");
    assert!(wait_until(Duration::from_secs(2), || !alive(daemon)));
    assert!(group_alive(pgid), "the run outlives its daemon");
    let (code, text) = jobs.itse(&["cancel", "run:1"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("STOPPED cancel jobs:serve — cancelled (signal "), "the new daemon re-adopted it: {text}");
    assert!(!group_alive(pgid));
}

#[test]
fn replacing_the_binary_under_a_live_daemon_still_runs_tests() {
    let jobs = Jobs::new(SCRIPTS);
    let copy = jobs.env.dir.join("itse-copy");
    let install = || {
        let _ = fs::remove_file(&copy);
        fs::copy(ITSE, &copy).unwrap();
    };
    // Another test's fork can hold the copy's write fd until its exec: ETXTBSY, briefly.
    let run = |args: &[&str]| {
        let mut out = jobs.cmd(copy.to_str().unwrap(), args).output();
        for _ in 0..50 {
            if !matches!(&out, Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
            out = jobs.cmd(copy.to_str().unwrap(), args).output();
        }
        let out = out.unwrap();
        (out.status.code(), stdout(&out))
    };
    install();
    assert_eq!(run(&["run", "jobs:serve"]).0, Some(4), "the copy's daemon has a live run");
    let daemon = jobs.env.daemon_json()["pid"].clone();
    install();
    let (code, text) = run(&["run", "jobs:hello"]);
    assert_eq!((code, text.lines().next()), (Some(0), Some("OK run jobs:hello — exit 0; run:2")), "{text}");
    assert_eq!(jobs.env.daemon_json()["pid"], daemon, "the old daemon served it, through /proc/self/exe");
    assert_eq!(run(&["cancel", "run:1"]).0, Some(0));
    let (code, text) = run(&["status"]);
    assert_eq!(code, Some(0), "{text}");
    assert_ne!(json!(daemon_pid(&text)), daemon, "with no run left, the next call restarts on the new binary");
}

#[test]
fn an_untrusted_workspace_runs_no_project_code() {
    let jobs = Jobs::new(SCRIPTS);
    let (code, text) = jobs.itse(&["run", "jobs:code"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED run jobs:code — it runs project code\nuntrusted      workspace "), "{text}");
    assert!(text.contains("project code runs in jobs:code; next (human): itse trust"), "{text}");
    assert!(!jobs.run_dir(1).exists(), "nothing started");
    assert_eq!(jobs.itse(&["trust"]).0, Some(0));
    let (code, text) = jobs.itse(&["run", "jobs:code"]);
    assert_eq!((code, text.lines().take(2).collect::<Vec<_>>()), (Some(0), vec!["OK run jobs:code — exit 0; run:1", "built"]));
}

#[test]
fn a_re_adopted_run_keeps_its_timeout() {
    let jobs = Jobs::new(SCRIPTS);
    let cli = jobs.cmd(ITSE, &["run", "jobs:capped"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let pgid = jobs.runner(1);
    let daemon = jobs.env.daemon_json()["pid"].as_i64().unwrap() as i32;
    signal(daemon, "KILL");
    assert!(wait_until(Duration::from_secs(2), || !alive(daemon)));
    drop(cli.wait_with_output());
    let (_, text) = jobs.itse(&["show", "run:1"]);
    assert!(text.starts_with("RUNNING run jobs:capped") || text.contains("timed out"), "the next daemon re-adopts it: {text}");
    assert!(wait_until(Duration::from_secs(8), || jobs.run_json(1)["exit"]["timed_out"] == true), "{}", jobs.run_json(1));
    assert!(wait_until(Duration::from_secs(2), || !group_alive(pgid)));
    let (_, text) = jobs.itse(&["show", "run:1"]);
    assert!(text.starts_with("FAIL run jobs:capped — timed out after "), "{text}");
}
