//! End-to-end tests: the built itse-bench, driven as a person drives it, on the starter tasks and
//! a whole run in both arms with the `fake` adapter (tests/fixtures), against the built itse.
//! Nothing here starts a real harness or spends anything.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, process};

use serde_json::Value;

const BENCH: &str = env!("CARGO_BIN_EXE_itse-bench");
const DIR: &str = env!("CARGO_MANIFEST_DIR");

/// A scratch dir under /tmp, short for itse's socket paths, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(format!("/tmp/itse-bench-{}-{}", process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(dir).unwrap())
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bench(args: &[&str]) -> Output {
    Command::new(BENCH).args(args).stdin(Stdio::null()).output().unwrap()
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// `cmd` with none of the caller's `GIT_*` vars. A git hook that runs the tests exports GIT_DIR (an
/// absolute one from a worktree), which would point the tests' git at the caller's repo.
fn no_git_env(mut cmd: Command) -> Command {
    for (key, _) in env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
        cmd.env_remove(key);
    }
    cmd
}

/// `bash SCRIPT WS` in `cwd` with WORKSPACE=WS; its exit code.
fn script(script: &Path, cwd: &Path, ws: &Path) -> i32 {
    let out = no_git_env(Command::new("bash")).arg(script).arg(ws).current_dir(cwd).env("WORKSPACE", ws).env_remove("CARGO_TARGET_DIR").stdin(Stdio::null()).output().unwrap();
    out.status.code().unwrap_or(-1)
}

/// The itse under test: this repo's debug build, built if needed.
fn itse() -> PathBuf {
    let repo = Path::new(DIR).parent().unwrap();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo).args(["build", "--quiet", "--locked", "--bin", "itse", "--manifest-path"]).arg(repo.join("Cargo.toml")).output().unwrap();
    assert!(out.status.success(), "building itse: {}", text(&out));
    repo.join("target/debug/itse")
}

/// Processes whose cmdline or environ names a path under `dir`.
fn processes_naming(dir: &Path) -> Vec<String> {
    let needle = format!("{}/", dir.display()).into_bytes();
    let me = process::id().to_string();
    let entries = fs::read_dir("/proc").unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
    entries
        .filter(|pid| pid.chars().all(|c| c.is_ascii_digit()) && *pid != me)
        .filter(|pid| ["cmdline", "environ"].iter().any(|f| fs::read(format!("/proc/{pid}/{f}")).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))))
        .collect()
}

#[test]
fn the_starter_tasks_check_and_their_solutions_pass_where_the_base_fails() {
    let tasks = Path::new(DIR).join("tasks");
    let out = bench(&["check-task", tasks.to_str().unwrap()]);
    let printed = text(&out);
    assert!(out.status.success(), "{printed}");
    assert!(printed.contains("ok rust-doc-control sha256:") && printed.contains("tags rust,docs,control\n"), "{printed}");
    assert!(printed.contains("ok rust-off-by-one sha256:"), "{printed}");
    for id in ["rust-off-by-one", "rust-doc-control"] {
        let tmp = Tmp::new();
        let (task, ws) = (tasks.join(id), tmp.0.join("ws"));
        fs::create_dir_all(&ws).unwrap();
        assert_eq!(script(&task.join("environment/setup.sh"), &tmp.0, &ws), 0, "{id}: setup");
        assert_ne!(script(&task.join("tests/test.sh"), &task, &ws), 0, "{id}: the base must fail");
        assert_eq!(script(&task.join("solution/solve.sh"), &ws, &ws), 0, "{id}: solve");
        assert_eq!(script(&task.join("tests/test.sh"), &task, &ws), 0, "{id}: the solution must pass");
        let manifest = fs::read_to_string(ws.join("Cargo.toml")).unwrap();
        fs::write(ws.join("Cargo.toml"), format!("{manifest}\n")).unwrap();
        assert_ne!(script(&task.join("tests/test.sh"), &task, &ws), 0, "{id}: a changed manifest fails");
        fs::write(ws.join("Cargo.toml"), manifest).unwrap();
        if id == "rust-off-by-one" {
            // A cargo config the agent adds (here one that runs no test binary) is a change outside src/.
            let host = String::from_utf8(Command::new("rustc").arg("-vV").output().unwrap().stdout).unwrap();
            let host = host.lines().find_map(|l| l.strip_prefix("host: ")).unwrap().to_string();
            fs::create_dir_all(ws.join(".cargo")).unwrap();
            fs::write(ws.join(".cargo/config.toml"), format!("[target.{host}]\nrunner = \"true\"\n")).unwrap();
            assert!(no_git_env(Command::new("git")).arg("-C").arg(&ws).args(["checkout", "-q", "src"]).status().unwrap().success());
            assert_ne!(script(&task.join("tests/test.sh"), &task, &ws), 0, "{id}: an unfixed src/ with a runner that always passes");
            fs::remove_dir_all(ws.join(".cargo")).unwrap();
            assert_eq!(script(&task.join("solution/solve.sh"), &ws, &ws), 0, "{id}: solve again");
        }
        let status = no_git_env(Command::new("git")).arg("-C").arg(&ws).args(["status", "--porcelain"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&status.stdout).contains("hidden"), "{id}: the hidden test is removed again");
    }
}

#[test]
fn check_task_fails_on_a_broken_task_and_names_it() {
    let tmp = Tmp::new();
    let task = tmp.0.join("broken");
    fs::create_dir_all(task.join("tests")).unwrap();
    fs::write(task.join("task.toml"), "version = 1.0\n[metadata]\n[agent]\ntimeout_sec = 1\n[verifier]\ntimeout_sec = 1\n").unwrap();
    fs::write(task.join("instruction.md"), "x").unwrap();
    let out = bench(&["check-task", task.to_str().unwrap()]);
    assert_eq!((out.status.code(), text(&out).as_str()), (Some(1), "error broken: missing environment/setup.sh, tests/test.sh, solution/solve.sh\n"));
    for f in ["environment/setup.sh", "tests/test.sh", "solution/solve.sh"] {
        fs::create_dir_all(task.join(f).parent().unwrap()).unwrap();
        fs::write(task.join(f), "true\n").unwrap();
    }
    let out = bench(&["check-task", task.to_str().unwrap()]);
    assert_eq!((out.status.code(), text(&out).as_str()), (Some(1), "error broken/task.toml: version must be a non-empty string\n"));
}

/// The private HOME's git config: a commit there would fail on signing, and an identity that must
/// not show up in trials.
const GITCONFIG: &str = "[commit]\n\tgpgsign = true\n[gpg]\n\tprogram = false\n[user]\n\tname = Not The Bench\n";

/// `itse-bench run` on `tasks` with `harness` from the fixtures, in `tmp` with a private HOME:
/// trials go to the relative `t`, results to `out`, the adapters file is the bare relative
/// `adapters.toml`. `itse` is put on PATH, not passed.
fn run_in(tmp: &Path, tasks: &Path, harness: &str, seed: &str, extra: &[(&str, &str)], more: &[&str]) -> Output {
    run_cmd(tmp, tasks, harness, seed, extra, more).output().unwrap()
}

fn run_cmd(tmp: &Path, tasks: &Path, harness: &str, seed: &str, extra: &[(&str, &str)], more: &[&str]) -> Command {
    let (home, pbin) = (tmp.join("home"), tmp.join("pbin"));
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".gitconfig"), GITCONFIG).unwrap();
    if !pbin.exists() {
        // itse and a shim of it on the caller's PATH, as `cargo install` and shims leave them.
        let (itse, link) = (itse(), std::os::unix::fs::symlink::<&Path, PathBuf>);
        fs::create_dir_all(&pbin).unwrap();
        link(&itse, pbin.join("itse")).unwrap();
        link(&itse, pbin.join("cargo")).unwrap();
    }
    let path = format!("{}:{}", pbin.display(), env::var("PATH").unwrap());
    let _ = std::os::unix::fs::symlink(format!("{DIR}/tests/fixtures/adapters.toml"), tmp.join("adapters.toml"));
    let args = ["run", "--tasks", tasks.to_str().unwrap(), "--harness", harness, "--adapters", "adapters.toml", "--reps", "1", "--seed", seed, "--out", "out", "--trials", "t"];
    let mut cmd = Command::new(BENCH);
    cmd.args(args).args(more).current_dir(tmp).env("HOME", &home).env("PATH", path).env("OLDPWD", tasks).envs(extra.iter().copied()).stdin(Stdio::null());
    cmd
}

#[test]
fn a_run_records_both_arms_of_a_trial_and_the_report_pairs_them() {
    let tmp = Tmp::new();
    let (trials, out) = (tmp.0.join("t"), tmp.0.join("out"));
    // Keys the harness must not see, one the adapter passes, and itse vars of the caller's.
    let env =
        [("ANTHROPIC_API_KEY", "sk-zqx-not-for-trials"), ("GH_TOKEN", "gh-zqx"), ("SECRET_X", "1"), ("FAKE_PASSED", "yes"), ("ITSE_USAGE", "off"), ("ITSE_SERVER_IDLE_S", "1")];
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-off-by-one"), "fake", "7", &env, &["--max-turns", "5"]);
    assert!(run.status.success(), "{}", text(&run));

    let results = fs::read_to_string(out.join("results.jsonl")).unwrap();
    let lines: Vec<Value> = results.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "{results}");
    let required = ["task", "task_sha", "arm", "rep", "harness", "harness_version", "model", "itse_version", "wall_ms", "exit", "timed_out", "check_exit", "pass", "shortstat"];
    let optional = ["fresh_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens", "cost_usd", "turns"];
    for line in &lines {
        for key in required {
            assert!(!line[key].is_null(), "{key} missing in {line}");
        }
        for key in optional.iter().chain(&["itse", "order", "seed", "run"]) {
            assert!(line.get(key).is_some(), "{key} missing in {line}");
        }
        assert_eq!(
            (line["task"].as_str(), line["harness"].as_str(), line["harness_version"].as_str(), line["rep"].as_u64()),
            (Some("rust-off-by-one"), Some("fake"), Some("fake 1.0"), Some(1))
        );
        assert_eq!((line["exit"].as_i64(), line["timed_out"].as_bool(), line["itse"]["arm_clean"].as_bool()), (Some(0), Some(false), Some(true)));
        assert_eq!((line["turns"].as_u64(), line["fresh_tokens"].as_u64(), line["cost_usd"].as_f64()), (Some(3), Some(1200), Some(0.0125)));
        assert!(line["cache_write_tokens"].is_null(), "a missing pointer is null");
        assert_eq!(line["task_sha"].as_str().unwrap().len(), 64);
        let caps = (line["max_turns"].as_u64(), line["permission_mode"].as_str(), line["agent_timeout_s"].as_f64());
        assert_eq!(caps, (Some(5), Some("bypassPermissions"), Some(600.0)), "an explicit --max-turns wins over the task's 30");
        assert_eq!(line["leftovers"], serde_json::json!([]));
        assert!(line["itse_version"].as_str().unwrap().starts_with("itse "));
    }
    assert_eq!(lines[0]["order"], lines[1]["order"]);
    let arm = |name: &str| lines.iter().find(|l| l["arm"] == name).unwrap();
    let (with, without) = (arm("with"), arm("without"));

    assert_eq!((with["pass"].as_bool(), with["check_exit"].as_i64()), (Some(true), Some(0)), "{with}");
    assert_eq!((with["shortstat"].as_str(), with["untracked"].as_u64()), (Some("1 file changed, 1 insertion(+), 1 deletion(-)"), Some(0)));
    let itse_block = &with["itse"];
    assert_eq!((&itse_block["used"], &itse_block["trusted_by"], &itse_block["skill"]), (&serde_json::json!(["rust"]), &serde_json::json!("bench"), &Value::Null));
    assert!(itse_block["usage"]["calls"].as_u64() >= Some(1), "the fake's itse status, from the trial's own log: {itse_block}");
    assert!(itse_block["usage"]["by_verb"]["status"].as_u64() >= Some(1), "{itse_block}");
    assert_eq!(itse_block["usage"]["by_verb"].get("use"), None, "the bench's own calls are not counted");
    let pid = itse_block["daemon"]["pid"].as_i64().expect("the daemon ran");
    assert!(itse_block["daemon"]["vmhwm_kb"].as_u64() > Some(0));
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    assert!(stat.is_empty() || stat.contains(") Z "), "the daemon stopped: {stat}");

    assert_eq!((without["pass"].as_bool(), without["shortstat"].as_str(), without["untracked"].as_u64()), (Some(false), Some(""), Some(1)), "NOTES.txt: {without}");
    assert_ne!(without["check_exit"].as_i64(), Some(0));
    assert_eq!((without["itse"]["usage"]["calls"].as_u64(), &without["itse"]["daemon"], &without["itse"]["trusted_by"]), (Some(0), &Value::Null, &Value::Null));

    let run_dir = fs::read_dir(&trials).unwrap().flatten().next().unwrap().path();
    let trial = |arm: &str, file: &str| fs::read_to_string(run_dir.join(format!("rust-off-by-one-1-{arm}/{file}"))).unwrap_or_default();
    assert!(trial("with", "itse-use.log").contains("OK use rust") && trial("with", "itse-trust.log").starts_with("OK trust"), "use and trust ran");
    assert!(trial("with", "itse-daemon.log").contains("stopped daemon pid"), "{}", trial("with", "itse-daemon.log"));
    // The skill's home is cfg/, so the harness's config dir must be cfg/.claude, where fake.sh wrote this.
    let skill_home = PathBuf::from(itse_block["skill_home"].as_str().unwrap());
    assert_eq!((skill_home.clone(), &without["itse"]["skill_home"]), (run_dir.join("rust-off-by-one-1-with/cfg"), &Value::Null));
    assert!(skill_home.join(".claude.seen").is_file());
    let with_seen = trial("with", "cfg/.claude.seen");
    assert!(with_seen.contains("\"command\":\"itse\"") && with_seen.contains("passed=yes\nblocked=\nleaked=\n"), "{with_seen}");
    assert!(with_seen.contains(&format!("itse={}", run_dir.join("rust-off-by-one-1-with/bin/itse").display())), "{with_seen}");
    let without_seen = trial("without", "cfg/.claude.seen");
    assert!(without_seen.starts_with("mcp={\"mcpServers\":{}}\n") && without_seen.contains("leaked=\nitse=\ncargo=/"), "{without_seen}");
    let cargo = without_seen.split("cargo=").nth(1).and_then(|c| c.lines().next()).unwrap();
    assert!(!cargo.contains("pbin") && !fs::canonicalize(cargo).unwrap().ends_with("itse"), "the without arm's cargo is not the itse shim: {cargo}");
    assert!(with_seen.contains("gituser=itse-bench\n") && without_seen.contains("gituser=itse-bench\n"), "the trial's git config, not HOME's");
    let home = format!("home={}\n", run_dir.join("rust-off-by-one-1-with/cfg").display());
    assert!(with_seen.contains(&home), "the harness's ~ is cfg/, as the skill's home is: {with_seen}");
    let home: Vec<_> = fs::read_dir(tmp.0.join("home")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(home, [".gitconfig"], "nothing landed in HOME");
    assert_eq!(processes_naming(&trials), Vec::<String>::new(), "no process of a trial is left");

    let report = bench(&["report", out.to_str().unwrap()]);
    let printed = text(&report);
    assert!(report.status.success(), "{printed}");
    assert!(printed.starts_with("harness fake (fake 1.0): 2 trials, 1 tasks\npass rate: with 1/1 (100%), without 0/1 (0%)\n"), "{printed}");
    assert!(printed.contains("\nrust-off-by-one      0          1             0        0\n"), "{printed}");
    assert!(printed.contains("sign test on 1 discordant pairs (1 only with, 0 only without): p = 1.0000\n"), "{printed}");
    assert!(printed.contains("10 tasks x 3 reps detect only large effects"), "{printed}");

    // Without the env the adapter requires, the next run refuses before it makes a trial.
    let refused = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-off-by-one"), "fake", "7", &[], &[]);
    let printed = text(&refused);
    assert_eq!(refused.status.code(), Some(2), "{printed}");
    assert!(printed.contains("[fake] needs FAKE_PASSED in the bench's env"), "{printed}");
    assert_eq!((fs::read_dir(&trials).unwrap().count(), fs::read_to_string(out.join("results.jsonl")).unwrap()), (1, results));
}

#[test]
fn a_harness_that_edits_the_verifier_stops_the_run_unrecorded() {
    let tmp = Tmp::new();
    let task = copy_task(&tmp.0, "rust-doc-control");
    let run = run_in(&tmp.0, &task, "tamper", "1", &[], &[]);
    assert!(!run.status.success() && text(&run).contains("changed during the trial"), "{}", text(&run));
    assert_eq!(fs::read_to_string(tmp.0.join("out/results.jsonl")).unwrap_or_default(), "", "no line for a tampered trial");
}

#[test]
fn a_harness_that_cannot_start_still_has_its_daemon_stopped() {
    let tmp = Tmp::new();
    // Seed 2 runs the with arm first, so use and trust have started the trial's daemon.
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "broken", "2", &[], &[]);
    let printed = text(&run);
    assert!(!run.status.success() && printed.contains("rust-doc-control rep 1 with") && printed.contains("/nonexistent-harness"), "{printed}");
    let run_dir = fs::read_dir(tmp.0.join("t")).unwrap().flatten().next().unwrap().path();
    let log = fs::read_to_string(run_dir.join("rust-doc-control-1-with/itse-daemon.log")).unwrap();
    assert!(log.contains("stopped daemon pid"), "{log}");
    assert_eq!(processes_naming(&tmp.0.join("t")), Vec::<String>::new(), "no process of the trial is left");
}

#[test]
fn the_bench_runs_no_git_config_the_agent_left_in_the_repo() {
    let tmp = Tmp::new();
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "fsmonitor", "1", &[], &[]);
    assert!(run.status.success(), "{}", text(&run));
    let run_dir = fs::read_dir(tmp.0.join("t")).unwrap().flatten().next().unwrap().path();
    let trial = run_dir.join("rust-doc-control-1-without");
    let config = fs::read_to_string(trial.join("ws/.git/config")).unwrap();
    assert!(config.contains("fsmonitor") && config.contains("filter-ran"), "the harness left them: {config}");
    assert!(!trial.join("fsmonitor-ran").exists() && !trial.join("filter-ran").exists(), "the shortstat ran without them");
    let results = fs::read_to_string(tmp.0.join("out/results.jsonl")).unwrap();
    let lines: Vec<Value> = results.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!((line["shortstat"].as_str(), line["untracked"].as_u64()), (Some("1 file changed, 1 insertion(+)"), Some(1)), "the filter hid nothing: {line}");
    }
}

#[test]
fn trials_under_a_dir_with_agent_instructions_are_refused() {
    let tmp = Tmp::new();
    fs::create_dir_all(tmp.0.join("proj")).unwrap();
    fs::write(tmp.0.join("proj/AGENTS.md"), "Always use itse.\n").unwrap();
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "fsmonitor", "1", &[], &["--trials", "proj/t"]);
    let printed = text(&run);
    assert_eq!(run.status.code(), Some(2), "{printed}");
    assert!(printed.contains(&format!("{} would reach every trial's agent", tmp.0.join("proj/AGENTS.md").display())), "{printed}");
}

#[test]
fn ctrl_c_ends_the_harness_and_the_daemon_and_records_nothing() {
    let tmp = Tmp::new();
    // Seed 2 runs the with arm first. As from a terminal: the bench leads its group, and the group gets SIGINT.
    let mut cmd = run_cmd(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "sleepy", "2", &[], &[]);
    let bench = cmd.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let (started, trial) = (Instant::now(), |run: PathBuf| run.join("rust-doc-control-1-with"));
    let run_dir = loop {
        let found = fs::read_dir(tmp.0.join("t")).into_iter().flatten().flatten().map(|e| e.path()).find(|r| trial(r.clone()).join("started").exists());
        if let Some(run) = found {
            break run;
        }
        assert!(started.elapsed() < Duration::from_secs(60), "the harness never started");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(Command::new("kill").args(["-INT", "--", &format!("-{}", bench.id())]).status().unwrap().success());
    // The worker holds the same stdout and stderr, so this waits for it too.
    let out = bench.wait_with_output().unwrap();
    let printed = text(&out);
    assert_eq!(out.status.signal(), Some(2), "{printed}");
    assert!(printed.contains("rust-doc-control rep 1 with: interrupted"), "{printed}");
    assert!(started.elapsed() < Duration::from_secs(25), "the harness's sleep 30 was ended");
    let log = fs::read_to_string(trial(run_dir).join("itse-daemon.log")).unwrap_or_default();
    assert!(log.contains("stopped daemon pid"), "{log}");
    assert_eq!(processes_naming(&tmp.0.join("t")), Vec::<String>::new(), "no process of the trial is left");
    assert_eq!(fs::read_to_string(tmp.0.join("out/results.jsonl")).unwrap_or_default(), "");
}

/// A copy of the starter task `id` under `tmp/tasks`, for a test to change.
fn copy_task(tmp: &Path, id: &str) -> PathBuf {
    let task = tmp.join("tasks").join(id);
    fs::create_dir_all(task.parent().unwrap()).unwrap();
    assert!(Command::new("cp").arg("-R").arg(Path::new(DIR).join("tasks").join(id)).arg(&task).status().unwrap().success());
    task
}

#[test]
fn a_use_entry_naming_no_extension_stops_the_run_before_any_trial() {
    let tmp = Tmp::new();
    let task = copy_task(&tmp.0, "rust-doc-control");
    let toml = fs::read_to_string(task.join("task.toml")).unwrap();
    fs::write(task.join("task.toml"), toml.replace("use = [\"rust\"]", "use = [\"rsut\"]")).unwrap();
    let run = run_in(&tmp.0, &task, "fsmonitor", "1", &[], &[]);
    let printed = text(&run);
    assert!(!run.status.success() && printed.contains("task rust-doc-control: itse use rsut exited 2"), "{printed}");
    let run_dir = fs::read_dir(tmp.0.join("t")).unwrap().flatten().next().unwrap().path();
    let made: Vec<_> = fs::read_dir(&run_dir).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != "tools").collect();
    assert_eq!(made, ["ext-check"], "no trial ran");
    assert!(!tmp.0.join("out/results.jsonl").exists());
}

#[test]
fn processes_a_harness_leaves_behind_are_killed_recorded_and_the_run_goes_on() {
    let tmp = Tmp::new();
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "escape", "1", &[], &[]);
    assert!(run.status.success(), "{}", text(&run));
    let results = fs::read_to_string(tmp.0.join("out/results.jsonl")).unwrap();
    let lines: Vec<Value> = results.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "the trial after the first is run and both are recorded: {results}");
    assert!(lines.iter().all(|l| l["leftovers"].as_array().is_some_and(|a| a.len() == 1)), "{results}");
    assert_eq!(processes_naming(&tmp.0.join("t")), Vec::<String>::new(), "the leftovers were killed");
    let report = text(&bench(&["report", tmp.0.join("out").to_str().unwrap()]));
    assert!(report.contains("\n2 trials left processes behind (killed)\n"), "{report}");
}

#[test]
fn a_harness_that_exits_at_once_is_a_harness_error_not_a_failed_pair() {
    let tmp = Tmp::new();
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "fails", "1", &[], &[]);
    assert!(run.status.success(), "{}", text(&run));
    let results = fs::read_to_string(tmp.0.join("out/results.jsonl")).unwrap();
    assert!(results.lines().all(|l| l.contains("\"exit\":7") && l.contains("\"turns\":null")), "{results}");
    assert!(text(&run).contains("rust-doc-control rep 1 with: harness error (exit 7; see harness.err)"), "{}", text(&run));
    // Claude Code's result for a failed login has a turn but no tokens: a harness error all the same.
    let run = run_in(&tmp.0, &Path::new(DIR).join("tasks/rust-doc-control"), "autherr", "1", &[], &[]);
    assert!(run.status.success() && text(&run).contains("rep 1 without: harness error (exit 1; see harness.err)"), "{}", text(&run));
    let report = text(&bench(&["report", tmp.0.join("out").to_str().unwrap()]));
    let errors = "2 trials, 0 tasks\n2 harness errors (exit not 0 or is_error, no tokens, no timeout), left out\n";
    assert!(report.contains(&format!("harness fails (fails 1.0): {errors}")) && report.contains(&format!("harness autherr (autherr 1.0): {errors}")), "{report}");
    assert!(report.contains("sign test on 0 discordant pairs"), "{report}");
}

#[test]
fn help_after_a_command_prints_the_usage_and_an_unknown_command_is_named() {
    for args in [&["--help"][..], &["run", "--help"], &["report", "-h"], &["check-task", "--help"], &["run", "--tasks", "x", "--help"]] {
        let out = bench(args);
        assert!(out.status.success() && text(&out).starts_with("usage: itse-bench run"), "{args:?}: {}", text(&out));
    }
    let out = bench(&["frob"]);
    assert_eq!((out.status.code(), text(&out).lines().next()), (Some(2), Some("itse-bench: unknown command 'frob'; itse-bench --help lists them")));
    let out = bench(&["run", "--tasks", "x", "--harness", "h", "--out", "o", "--reps", "abc"]);
    assert_eq!((out.status.code(), text(&out).as_str()), (Some(2), "itse-bench: --reps \"abc\": invalid digit found in string\n"));
    let out = bench(&["report"]);
    assert!(out.status.code() == Some(2) && text(&out).starts_with("itse-bench: report needs a path"), "{}", text(&out));
}
