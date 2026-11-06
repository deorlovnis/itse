use super::*;

#[test]
fn xorshift_gives_marsaglias_sequence() {
    let mut x = 1;
    assert_eq!([xorshift(&mut x), xorshift(&mut x), xorshift(&mut x)], [1_082_269_761, 1_152_992_998_833_853_505, 11_177_516_664_432_764_457]);
}

#[test]
fn a_timed_out_command_loses_its_whole_group() {
    let started = Instant::now();
    let log = env::temp_dir().join(format!("itse-bench-group-{}.log", process::id()));
    let script = "sleep 30 & echo $! > \"$0.pid\"; exec sleep 30";
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script, log.to_str().unwrap()]).stdout(Stdio::null());
    let (exit, timed_out) = group(&mut cmd, Duration::from_millis(300), true).unwrap();
    assert_eq!((exit, timed_out), (128 + 15, true), "SIGTERM ended it");
    assert!(started.elapsed() < Duration::from_secs(4));
    let bg: i32 = fs::read_to_string(log.with_extension("log.pid")).unwrap().trim().parse().unwrap();
    assert!(fs::read_to_string(format!("/proc/{bg}/stat")).map_or(true, |s| s.contains(") Z ")), "the background sleep went too");
    let _ = fs::remove_file(log.with_extension("log.pid"));
}

#[test]
fn a_group_that_ignores_sigterm_gets_sigkill_after_5_s() {
    let started = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "trap '' TERM; sleep 30"]);
    let (exit, timed_out) = group(&mut cmd, Duration::from_millis(100), true).unwrap();
    assert_eq!((exit, timed_out), (128 + 9, true));
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(8), "{took:?}");
}

#[test]
fn a_leader_that_exits_takes_its_background_children_along() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 30 & exit 3"]);
    let started = Instant::now();
    let (exit, timed_out) = group(&mut cmd, Duration::from_secs(10), true).unwrap();
    assert_eq!((exit, timed_out), (3, false));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn the_with_arm_puts_bin_and_shims_first_and_both_get_trial_homes() {
    let base = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string()), ("HOME".to_string(), "/h".to_string())]);
    let with = trial_env(&base, Path::new("/t/x-1-with"), true);
    assert_eq!(with["PATH"], "/t/x-1-with/bin:/t/x-1-with/data/itse/bin:/usr/bin:/bin");
    assert_eq!((with["XDG_STATE_HOME"].as_str(), with["XDG_CONFIG_HOME"].as_str(), with["ITSE_SERVER_IDLE_S"].as_str()), ("/t/x-1-with/state", "/t/x-1-with/cfg", "60"));
    let without = trial_env(&base, Path::new("/t/x-1-without"), false);
    assert_eq!((without["PATH"].as_str(), without["XDG_DATA_HOME"].as_str(), without["HOME"].as_str()), ("/usr/bin:/bin", "/t/x-1-without/data", "/t/x-1-without/cfg"));
    assert_eq!(with["HOME"], "/t/x-1-with/cfg", "no ~/CLAUDE.md, ~/.bashrc or the like of the user's reaches a trial");
    for (env, dir) in [(&with, "/t/x-1-with"), (&without, "/t/x-1-without")] {
        assert_eq!(env["GIT_CONFIG_NOSYSTEM"], "1", "setup, the verifier and the harness read no system git config");
        assert_eq!(env["GIT_CONFIG_GLOBAL"], format!("{dir}/gitconfig"), "nor the user's ~/.gitconfig");
    }
}

#[test]
fn the_usage_summary_counts_only_lines_past_the_offset() {
    let log = env::temp_dir().join(format!("itse-bench-usage-{}.jsonl", process::id()));
    let before = "{\"ev\":\"call\",\"verb\":\"use\",\"outcome\":\"ok\",\"bytes\":10}\n";
    let lines = [
        "{\"ev\":\"init\",\"client\":\"claude-code\",\"list_bytes\":3100}",
        "{\"ev\":\"call\",\"verb\":\"test\",\"outcome\":\"fail\",\"bytes\":900}",
        "{\"ev\":\"call\",\"verb\":\"test\",\"outcome\":\"ok\",\"bytes\":100}",
        "{\"ev\":\"call\",\"verb\":\"find\",\"outcome\":\"error\",\"bytes\":40}",
        "not json",
    ];
    fs::write(&log, format!("{before}{}\n", lines.join("\n"))).unwrap();
    let got = usage(&log, before.len() as u64);
    assert_eq!(got, json!({"calls": 3, "by_verb": {"find": 1, "test": 2}, "errors": 1, "bytes": 1040, "list_bytes": 3100}));
    assert_eq!(usage(&log.with_extension("missing"), 0)["calls"], 0);
    let _ = fs::remove_file(&log);
}

#[test]
fn the_bench_env_keeps_an_allowlist_and_drops_secrets_and_harness_vars() {
    let planted = [
        "ITSE_X",
        "XDG_STATE_HOME",
        "CLAUDE_CONFIG_DIR",
        "ANTHROPIC_API_KEY",
        "GH_TOKEN",
        "PWD",
        "OLDPWD",
        "_",
        "PATH",
        "HOME",
        "LC_ALL",
        "https_proxy",
        "NO_PROXY",
        "RUSTUP_HOME",
    ];
    let env = base_env(planted.iter().map(|k| (k.into(), "v".into())));
    assert_eq!(env.keys().map(String::as_str).collect::<Vec<_>>(), ["HOME", "LC_ALL", "NO_PROXY", "PATH", "RUSTUP_HOME", "https_proxy"]);
}

#[test]
fn path_dirs_that_offer_itse_are_masked_and_their_other_tools_kept() {
    let tmp = env::temp_dir().join(format!("itse-bench-mask-{}", process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let (real, shims, other, tools) = (tmp.join("real"), tmp.join("shims"), tmp.join("other"), tmp.join("tools"));
    for (dir, name) in [(&real, "itse"), (&real, "git"), (&other, "cargo"), (&other, "itse-not")] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
    }
    let itse = real.join("itse");
    fs::create_dir_all(&shims).unwrap();
    symlink(&itse, shims.join("cargo")).unwrap();
    fs::hard_link(&itse, other.join("copy")).unwrap();
    let path = format!("{}:{}:/nonexistent", shims.display(), real.display());
    assert_eq!(mask(&path, &itse, &tools).unwrap(), format!("{}/0:/nonexistent", tools.display()), "both dirs swapped for one tools dir");
    let mut kept: Vec<String> = fs::read_dir(tools.join("0")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    kept.sort();
    assert_eq!(kept, ["git"], "the shim cargo and itse itself are gone");
    assert_eq!(fs::read_link(tools.join("0/git")).unwrap(), real.join("git"));
    assert!(is_itse(&other.join("copy"), &itse) && !is_itse(&other.join("cargo"), &itse) && !is_itse(&other.join("itse-not"), &itse));
    let tools = tmp.join("tools2");
    assert_eq!(mask(&other.display().to_string(), &itse, &tools).unwrap(), format!("{}/0", tools.display()), "a hard link counts too");
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn masked_dirs_apart_on_path_keep_their_places_so_tools_resolve_as_before() {
    let tmp = env::temp_dir().join(format!("itse-bench-mask-apart-{}", process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let (a, b, c, tools) = (tmp.join("a"), tmp.join("b"), tmp.join("c"), tmp.join("tools"));
    for (dir, name) in [(&a, "itse"), (&b, "foo"), (&c, "foo"), (&c, "cargo")] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "").unwrap();
    }
    symlink(a.join("itse"), c.join("itse")).unwrap();
    let path = format!("{}:{}:{}", a.display(), b.display(), c.display());
    let t = tools.display();
    assert_eq!(mask(&path, &a.join("itse"), &tools).unwrap(), format!("{t}/0:{}:{t}/1", b.display()), "b's foo still comes before c's");
    assert!(fs::read_dir(tools.join("0")).unwrap().next().is_none());
    assert_eq!((fs::read_link(tools.join("1/foo")).unwrap(), fs::read_link(tools.join("1/cargo")).unwrap()), (c.join("foo"), c.join("cargo")));
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn reap_kills_only_processes_with_the_trials_env_not_ones_that_name_its_dir() {
    let dir = env::temp_dir().join(format!("itse-bench-reap-{}", process::id()));
    // A `tail -f` on a trial log, as a user would run it: the dir is in its cmdline only.
    let mut onlooker = Command::new("sh").args(["-c", "sleep 30; :"]).arg(dir.join("setup.log")).spawn().unwrap();
    let mut left = Command::new("sleep").arg("30").env("XDG_STATE_HOME", dir.join("state")).spawn().unwrap();
    let mut other = Command::new("sleep").arg("30").env("XDG_STATE_HOME", dir.join("state/x")).spawn().unwrap();
    assert_eq!(reap(&dir), [left.id() as i32]);
    assert!(left.wait().unwrap().signal() == Some(9) && onlooker.try_wait().unwrap().is_none() && other.try_wait().unwrap().is_none());
    let _ = (onlooker.kill(), other.kill(), onlooker.wait(), other.wait());
}

#[test]
fn the_progress_line_names_why_a_trial_failed() {
    let v = |line: Value| verdict(&line);
    assert_eq!(v(json!({"pass": true, "exit": 0})), "pass");
    assert_eq!(v(json!({"pass": false, "exit": 7, "turns": null})), "harness error (exit 7; see harness.err)");
    assert_eq!(v(json!({"pass": false, "exit": 143, "timed_out": true, "output_tokens": 5})), "fail (timed out)");
    assert_eq!(v(json!({"pass": false, "exit": 0, "timed_out": false, "check_exit": 101})), "fail (verifier exit 101)");
}
