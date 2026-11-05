use std::collections::BTreeMap;

use super::*;
use crate::manifest::{Layer, Manifest, parse_ext};
use crate::sys::tests::TempDir;

/// A resolved workspace using one extension `t` with these actions, and package `tool` at /store/tool.
fn resolved(actions: &str) -> Resolved {
    let e = parse_ext(&format!(r#"{{"itse_ext": 1, "id": "t", "actions": {actions}}}"#), "test").unwrap();
    let layer = Layer { uses: [("t".to_string(), BTreeMap::new())].into(), ..Layer::default() };
    let manifest = Manifest { exts: [("t".to_string(), (e, "added"))].into(), dev: Some((PathBuf::new(), layer)), team: None, team_bytes: vec![], notes: vec![] };
    Resolved { manifest, states: vec![], launches: vec![], notes: vec![], trust_hash: String::new(), trusted: true, roots: [("tool".into(), "/store/tool".into())].into() }
}

fn planned(actions: &str, label: &str, inputs: &[(&str, &str)], extra: &[&str]) -> Result<Plan> {
    let inputs = inputs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    plan(&resolved(actions), Path::new("/w"), Path::new("/cache/itse"), label, &inputs, &extra.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn argvs(plan: &Plan) -> Vec<Vec<&str>> {
    plan.steps.iter().map(|s| s.0.iter().map(String::as_str).collect()).collect()
}

#[test]
fn a_process_task_fills_its_templates_and_takes_the_extra_args() {
    let a = r#"[{"label": "t:gen", "type": "process", "command": "${pkg:tool}/bin/gen", "args": ["--out=${cache}/gen", "${workspaceFolder}"],
        "options": {"cwd": "sub", "env": {"GEN_HOME": "${pkg:tool}"}}, "timeout_s": 30, "ready": "port (\\d+)"}]"#;
    let p = planned(a, "t:gen", &[], &["--", "x y"]).unwrap();
    assert_eq!(argvs(&p), [["/store/tool/bin/gen", "--out=/cache/itse/gen", "/w", "--", "x y"]]);
    assert_eq!((p.steps[0].1.as_path(), p.env["GEN_HOME"].as_str()), (Path::new("/w/sub"), "/store/tool"));
    assert_eq!((p.timeout_s, p.ready.as_ref().map(|r| r.as_str()), p.code), (Some(30), Some("port (\\d+)"), false));
}

#[test]
fn depends_on_steps_come_first_depth_first_and_in_order() {
    let a = r#"[{"label": "a", "command": "a", "dependsOn": ["b", "c"], "executes_project_code": true}, {"label": "b", "command": "b", "dependsOn": "d"},
        {"label": "c", "command": "c", "args": ["${input:x}"]}, {"label": "d", "command": "d"}, {"label": "all", "dependsOn": ["d"]}]"#;
    let p = planned(a, "a", &[("x", "1")], &["--extra"]).unwrap();
    assert_eq!(argvs(&p), [vec!["d"], vec!["b"], vec!["c", "1"], vec!["a", "--extra"]], "extra args go to the label that was asked for");
    assert!(p.code, "one step runs project code");
    assert_eq!(argvs(&planned(a, "all", &[], &[]).unwrap()), [["d"]], "a task with only dependsOn adds no step of its own");
}

#[test]
fn a_depends_on_cycle_and_an_unknown_label_are_errors() {
    let a = r#"[{"label": "a", "command": "a", "dependsOn": "b"}, {"label": "b", "command": "b", "dependsOn": ["a"]}]"#;
    assert_eq!(planned(a, "a", &[], &[]).unwrap_err().to_string(), "dependsOn cycle: a → b → a");
    assert_eq!(planned(a, "zz", &[], &[]).unwrap_err().to_string(), "no action 'zz' in the extensions in use; declared: a, b");
}

#[test]
fn a_missing_input_names_the_flag_and_an_empty_one_drops_its_args() {
    let a = r#"[{"label": "t:test", "command": "cargo", "args": ["test", "${target_args}", "${filter_args}"], "target_args": ["-p", "${input:target}"],
        "filter_args": ["--", "${input:filter}"]}, {"label": "t:one", "command": "run", "args": ["${input:target}:test"]}]"#;
    assert_eq!(argvs(&planned(a, "t:test", &[], &[]).unwrap()), [["cargo", "test"]]);
    assert_eq!(argvs(&planned(a, "t:test", &[("filter", "")], &[]).unwrap()), [["cargo", "test"]]);
    assert_eq!(argvs(&planned(a, "t:test", &[("target", "core"), ("filter", "it_works")], &[]).unwrap()), [["cargo", "test", "-p", "core", "--", "it_works"]]);
    let missing = planned(a, "t:one", &[], &[]).unwrap_err();
    assert_eq!(format!("{missing:#}"), "t:one takes --input name=value: nothing fills ${input:target} in '${input:target}:test'");
}

#[test]
fn a_shell_task_runs_sh_on_its_command_line_with_each_arg_quoted() {
    let a = r#"[{"label": "s", "type": "shell", "command": "echo $HOME && ls", "args": ["plain-1.0", "two words", "it's", ""]}]"#;
    assert_eq!(argvs(&planned(a, "s", &[], &[]).unwrap()), [["/bin/sh", "-c", r#"echo $HOME && ls 'plain-1.0' 'two words' 'it'\''s' ''"#]]);
}

/// A run whose output is appended by the test.
fn fake_run(tmp: &TempDir) -> Arc<Run> {
    fs::write(tmp.0.join("out.log"), "").unwrap();
    Arc::new(Run { handle: "run:1".into(), dir: tmp.0.clone(), ..Run::default() })
}

fn append(tmp: &TempDir, text: &str) {
    OpenOptions::new().append(true).open(tmp.0.join("out.log")).unwrap().write_all(text.as_bytes()).unwrap();
}

#[test]
fn waiting_for_ready_returns_its_captures_from_a_whole_line() {
    let tmp = TempDir::new("itse-actions");
    let run = fake_run(&tmp);
    append(&tmp, "starting\nlistening on port 47");
    let (re, cancel) = (Regex::new(r"port (\d+)").unwrap(), Cancel::default());
    assert_eq!(wait(&run, Some(&re), Duration::from_millis(200), &cancel), None, "half a line is not matched yet");
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        append(&tmp, "11 at 127.0.0.1\nmore\n");
        tmp
    });
    assert_eq!(wait(&run, Some(&re), Duration::from_secs(5), &cancel), Some(vec!["4711".into()]));
    drop(writer.join());
    let started = Instant::now();
    cancel.cancel();
    assert_eq!(wait(&run, Some(&re), Duration::from_secs(5), &cancel), None);
    *lock(&run.done) = true;
    assert_eq!(wait(&run, None, Duration::from_secs(5), &Cancel::default()), None);
    assert!(started.elapsed() < Duration::from_secs(1), "a cancel or the end returns at once");
}

#[test]
fn a_run_reads_as_running_then_as_it_ended() {
    let tmp = TempDir::new("itse-actions");
    fs::write(tmp.0.join("out.log"), "compiling\n   Finished in 1s\n").unwrap();
    let mut rec = RunRecord { spec: RunSpec { label: "t:build".into(), ..RunSpec::default() }, ..RunRecord::default() };
    rec.save(&tmp.0).unwrap();
    let head = |rec: &RunRecord| {
        rec.save(&tmp.0).unwrap();
        let o = outcome("run", "run:3", &tmp.0);
        (o.head(), o.notes)
    };
    let (running, notes) = head(&rec);
    assert!(running.starts_with("RUNNING run t:build — 0 s so far; last line \"   Finished in 1s\"; run:3"), "{running}");
    assert_eq!(notes, ["compiling", "   Finished in 1s", &format!("log: {}", tmp.0.join("out.log").display())]);
    rec.exit = Some(RunExit { code: Some(0), ms: 1500, ..RunExit::default() });
    assert_eq!(head(&rec).0, "OK run t:build — exit 0; 1.5 s; run:3");
    rec.exit = Some(RunExit { code: Some(101), ..RunExit::default() });
    assert_eq!(head(&rec).0, "FAIL run t:build — exit 101; run:3");
    rec.exit = Some(RunExit { signal: Some(9), cancelled: true, ..RunExit::default() });
    assert_eq!(head(&rec).0, "STOPPED run t:build — cancelled (signal 9); run:3");
    rec.exit = Some(RunExit { signal: Some(15), cancelled: true, timed_out: true, ms: 1000, ..RunExit::default() });
    assert_eq!(head(&rec).0, "FAIL run t:build — timed out after 1.0 s; 1.0 s; run:3");
}

#[test]
fn a_run_record_grows_from_spec_to_exit() {
    let spec = r#"{"spec": {"steps": [[["cargo", "test"], "/w"]], "limits": {}, "label": "cargo:test", "started_at": "t"}}"#;
    let mut run: RunRecord = serde_json::from_str(spec).unwrap();
    assert!(run.pid.is_none() && run.exit.is_none());
    run.exit = Some(RunExit { signal: Some(9), cancelled: true, ..RunExit::default() });
    let back: RunRecord = serde_json::from_str(&serde_json::to_string(&run).unwrap()).unwrap();
    assert_eq!(back.exit, run.exit);
    assert!(!serde_json::to_string(&run).unwrap().contains("env"), "the env is never written");
}

#[test]
fn a_ready_line_written_just_before_the_end_still_counts() {
    let tmp = TempDir::new("itse-actions");
    let run = fake_run(&tmp);
    append(&tmp, "listening on port 4711\n");
    *lock(&run.done) = true;
    assert_eq!(wait(&run, Some(&Regex::new(r"port (\d+)").unwrap()), Duration::from_secs(5), &Cancel::default()), Some(vec!["4711".into()]));
}

fn host(tmp: &TempDir) -> Arc<Host> {
    let vars: BTreeMap<&str, std::ffi::OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = crate::workspace::Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let ws = crate::workspace::Workspace::find(Some(tmp.0.clone()), &tmp.0).unwrap();
    let socket = ws.socket(&dirs, &tmp.0).unwrap();
    Arc::new(Host::new(ws, dirs, socket, (1, 2)))
}

#[test]
fn a_runner_that_ends_without_its_exit_keeps_its_own_status_unless_it_was_re_adopted() {
    let tmp = TempDir::new("itse-actions");
    let host = host(&tmp);
    let ended = |status: Option<i32>, stopping: bool| {
        let dir = tmp.0.join("run");
        sys::private_dir(&dir).unwrap();
        RunRecord { spec: RunSpec { label: "t:x".into(), ..RunSpec::default() }, ..RunRecord::default() }.save(&dir).unwrap();
        let run = Run { handle: "run:1".into(), dir: dir.clone(), stopping: AtomicBool::new(stopping), ..Run::default() };
        finish(&host, &run, status.map(ExitStatus::from_raw));
        assert!(run.ended(Duration::ZERO));
        outcome("run", "run:1", &dir).head()
    };
    assert_eq!(ended(Some(2 << 8), false), "FAIL run t:x — exit 2; run:1", "a runner that failed on its own is no cancel");
    assert_eq!(ended(Some(9), true), "STOPPED run t:x — cancelled (signal 9); run:1");
    assert_eq!(ended(None, false), "STOPPED run t:x — cancelled (signal 9); run:1", "a re-adopted runner gone without its exit was killed");
}

#[test]
fn a_cancel_signals_only_a_step_still_in_the_runs_group() {
    let sleeper = || host::spawn(Command::new("sleep").arg("30"), &Limits::default(), false, 0).unwrap();
    let (mut leader, mut stranger) = (sleeper(), sleeper());
    let tmp = TempDir::new("itse-actions");
    let pid = Some(RunPid { runner_pid: leader.id() as i32, child_pid: stranger.id() as i32, start_time: 0 });
    RunRecord { pid, ..RunRecord::default() }.save(&tmp.0).unwrap();
    let start_time = sys::proc_stat(leader.id() as i32).unwrap().start_time;
    let run = Run { handle: "run:1".into(), dir: tmp.0.clone(), pgid: leader.id() as i32, start_time, ..Run::default() };
    *lock(&run.done) = true;
    // Reaped as soon as it dies, as the daemon does with its runners; a zombie still counts as a member.
    let reaper = thread::spawn(move || leader.wait().unwrap().signal());
    stop_run(&run);
    assert_eq!(reaper.join().unwrap(), Some(sys::SIGINT), "the SIGINT went to the run's own leader instead");
    assert_eq!(stranger.try_wait().unwrap(), None, "a pid on record that left the group gets nothing");
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}
