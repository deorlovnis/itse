//! The resolver end to end: detection, `use`/`unuse`, layers, trust across worktrees, human-only
//! verbs, and that no verb writes into the workspace.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;

use super::*;

/// git with this test's home and no system config, or false when git fails or is missing.
pub(super) fn git(env: &Env, dir: &Path, args: &[&str]) -> bool {
    let mut cmd = Command::new("git");
    // A git hook running the tests exports GIT_DIR or GIT_INDEX_FILE; they must not reach the temp repos.
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
        cmd.env_remove(key);
    }
    let ident = [("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"), ("GIT_CONFIG_NOSYSTEM", "1")];
    cmd.args(args).current_dir(dir).env("HOME", env.dir.join("home")).envs(ident).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.status().is_ok_and(|s| s.success())
}

/// A committed repo in the work folder holding `files`; false without git.
pub(super) fn repo(env: &Env, files: &[(&str, &str)]) -> bool {
    let root = env.work();
    for (name, body) in files {
        fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
        fs::write(root.join(name), body).unwrap();
    }
    git(env, &root, &["init", "-q", "-b", "main"]) && git(env, &root, &["add", "-A"]) && git(env, &root, &["commit", "-q", "-m", "init"])
}

/// Every path under `root` with its size, mtime and inode (directories included).
pub(super) fn tree(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime, u64)> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                todo.push(entry.path());
            }
            out.insert(entry.path(), (meta.len(), meta.modified().unwrap(), meta.ino()));
        }
    }
    out
}

/// `itse ARGS` in the work folder: (exit code, stdout, stderr).
fn run(env: &Env, args: &[&str]) -> (Option<i32>, String, String) {
    let out = env.itse(&env.work(), args);
    (out.status.code(), stdout(&out), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn a_fresh_repo_shows_suggestions_with_itse_use_as_next() {
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    fs::write(env.work().join("Cargo.toml"), "[package]\n").unwrap();
    let (code, text, _) = run(&env, &["status"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with(&format!("OK status {} — 1 suggested; daemon pid ", env.work().display())), "{text}");
    assert_eq!(text.lines().nth(1), Some("suggested      rust                 Cargo.toml found; next (agent): itse use rust"));
    let mut mcp = env.mcp(&[]);
    mcp.handshake("t");
    let (is_error, text) = mcp.call("problems_symbols", json!({"verb": "status"}));
    assert!(!is_error && text.contains(r#"Cargo.toml found; next (agent): test_build_run {verb: "use", args: ["rust"]}"#), "{text}");
}

#[test]
fn use_and_unuse_write_only_the_developer_layer() {
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    run(&env, &["verbs"]); // the usage log's salt and first line, so they don't count below
    let before = tree(&env.dir);
    let (code, text, err) = run(&env, &["use", "jdk", "version=25.0.5+8"]);
    let layer = fs::canonicalize(env.dir.join("config")).unwrap().join("itse/workspaces/work.json");
    assert_eq!(code, Some(0), "{text}{err}");
    assert!(text.starts_with(&format!("OK use jdk — {}\nmissing        jdk ", layer.display())), "{text}");
    assert!(text.ends_with("use.jdk = {\"version\":\"25.0.5+8\"}\n"), "{text}");
    let changed: Vec<PathBuf> = tree(&env.dir).into_iter().filter(|(p, m)| before.get(p) != Some(m)).map(|(p, _)| p).collect();
    let usage = env.dir.join("state/itse/usage.jsonl");
    let expected = [env.dir.join("config"), env.dir.join("config/itse"), env.dir.join("config/itse/workspaces"), env.dir.join("config/itse/workspaces/work.json")];
    assert_eq!(changed.iter().filter(|p| **p != usage && **p != env.dir.join("state/itse")).collect::<Vec<_>>(), expected.iter().collect::<Vec<_>>());
    let doc: Value = serde_json::from_slice(&fs::read(&layer).unwrap()).unwrap();
    assert_eq!(doc, json!({"itse": 1, "repo": format!("{}/.git", env.work().display()), "use": {"jdk": {"version": "25.0.5+8"}}}));
    let (code, text, _) = run(&env, &["unuse", "jdk"]);
    assert_eq!((code, text.lines().last()), (Some(0), Some("removed use.jdk")));
    assert!(!layer.exists(), "the empty layer is removed");
    let (code, _, err) = run(&env, &["unuse", "jdk"]);
    assert_eq!((code, err.as_str()), (Some(2), ""), "the error is the result's first line");
    assert!(env.daemons().is_empty(), "use and unuse run without the daemon");
}

#[test]
fn a_new_worktree_inherits_the_developer_layer_and_trust() {
    let env = Env::new();
    if !repo(&env, &[("README", "x\n")]) {
        return eprintln!("skipped: needs git");
    }
    let wt = env.dir.join("wt");
    assert!(git(&env, &env.work(), &["worktree", "add", "-q", wt.to_str().unwrap()]));
    let wt = fs::canonicalize(wt).unwrap();
    assert_eq!(run(&env, &["use", "rust"]).0, Some(0));
    assert_eq!(run(&env, &["trust"]).0, Some(0));
    let out = env.itse(&wt, &["status"]);
    let text = stdout(&out);
    assert!(text.starts_with(&format!("OK status {} — daemon pid ", wt.display())), "{text}");
    assert!(text.contains("\nready          workspace            trusted\n"), "trust is the repo's: {text}");
    let layer = fs::canonicalize(env.dir.join("config")).unwrap().join("itse/workspaces/work.json");
    assert!(text.contains(&format!("\nlayer: {} for repo {}\n", layer.display(), env.work().display())), "{text}");
    assert_eq!(env.daemons().len(), 2, "the worktree has a daemon of its own");
}

#[test]
fn a_team_layer_is_read_but_never_written() {
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    let team = env.work().join(".itse/itse.json");
    fs::create_dir_all(team.parent().unwrap()).unwrap();
    fs::write(&team, r#"{"itse": 1, "use": {"node": {}}}"#).unwrap();
    let before = tree(&env.work().join(".itse"));
    // A node of our own on PATH, so the node row is `unpinned` whatever the host has.
    let bin = env.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("node"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(bin.join("node"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = env.command(&env.work(), &["status"]).env("PATH", format!("{}:/usr/bin:/bin", bin.display())).output().unwrap();
    let (code, text) = (out.status.code(), stdout(&out));
    assert_eq!(code, Some(0));
    assert!(text.starts_with(&format!("UNREADY status {} — 1 of 2 not ready;", env.work().display())), "{text}");
    assert!(text.contains("untrusted      workspace            project code runs in the team layer .itse/; next (human): itse trust in a terminal"), "{text}");
    assert!(text.contains("\nteam layer: .itse/itse.json, read-only\n"), "{text}");
    let (code, text, _) = run(&env, &["unuse", "node"]);
    assert_eq!((code, text.as_str()), (Some(2), "ERROR unuse — node is in the team layer .itse/itse.json, which itse never writes\n"));
    assert_eq!(run(&env, &["use", "rust"]).0, Some(0));
    assert_eq!(run(&env, &["trust"]).0, Some(0));
    assert!(run(&env, &["status"]).1.contains("\nready          workspace            trusted\n"));
    assert_eq!(tree(&env.work().join(".itse")), before, "the team layer is untouched");
}

#[test]
fn human_only_verbs_record_the_actor_and_consent_needs_a_terminal() {
    let env = Env::new();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    let ext = env.dir.join("hello.json");
    let hello =
        r#"{"itse_ext": 1, "id": "hello", "packages": {"hello-ls": {"purl": "pkg:generic/x/hello-ls", "install": "path", "consents": [{"id": "region", "kind": "choice"}]}}}"#;
    fs::write(&ext, hello).unwrap();
    let (code, text, _) = run(&env, &["ext", "add", ext.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains(") by non-tty\n"), "{text}");
    assert_eq!(run(&env, &["trust"]).0, Some(0));
    let trust: Value = serde_json::from_slice(&fs::read(env.dir.join(format!("state/itse/repo/{}/trust.json", rid(&env)))).unwrap()).unwrap();
    assert_eq!(trust["actor"], "non-tty");
    let (code, text, _) = run(&env, &["consent", "hello-ls", "--region", "eu"]);
    assert_eq!((code, text.as_str()), (Some(3), "NEEDS_HUMAN consent hello-ls — a human runs this; next (human): itse consent hello-ls in a terminal\n"));
    assert!(!env.dir.join("state/itse/consents.json").exists());
    if Command::new("script").arg("--version").output().is_err() {
        return eprintln!("skipped the terminal half: needs script(1)");
    }
    let in_tty = |line: &str| env.command_of("script", &env.work(), &["-qec", &format!("{ITSE} {line}"), "/dev/null"]).output().unwrap();
    let out = in_tty("consent hello-ls --region eu");
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).starts_with("OK consent hello-ls — recorded region by tty"), "{}", stdout(&out));
    let consents: Value = serde_json::from_slice(&fs::read(env.dir.join("state/itse/consents.json")).unwrap()).unwrap();
    assert_eq!((consents["hello-ls"]["region"].as_str(), consents["hello-ls"]["actor"].as_str()), (Some("eu"), Some("tty")));
    assert_eq!(in_tty("trust").status.code(), Some(0));
    let trust: Value = serde_json::from_slice(&fs::read(env.dir.join(format!("state/itse/repo/{}/trust.json", rid(&env)))).unwrap()).unwrap();
    assert_eq!(trust["actor"], "tty");
}

/// The repo id of the work folder, from `status`.
fn rid(env: &Env) -> String {
    let text = run(env, &["status"]).1;
    text.split("(rid ").nth(1).and_then(|r| r.get(..12)).unwrap_or_else(|| panic!("no rid in {text}")).to_string()
}

#[test]
fn nothing_writes_inside_the_workspace() {
    let env = Env::new();
    let files = [("Cargo.toml", "[package]\nname = \"w\"\n"), ("package.json", "{\"engines\": {\"node\": \">=22\"}}\n"), ("src/lib.rs", "pub fn f() {}\n")];
    if !repo(&env, &files) {
        return eprintln!("skipped: needs git");
    }
    let ext = env.dir.join("hello.json");
    fs::write(&ext, r#"{"itse_ext": 1, "id": "hello"}"#).unwrap();
    let before = tree(&env.work());
    assert!(before.contains_key(&env.work().join(".git/index")));
    let mut calls: Vec<Vec<&str>> = vec![
        vec!["status"],
        vec!["status", "--hosts"],
        vec!["use", "rust"],
        vec!["use", "node", "install=path"],
        vec!["use", "kotlin"],
        vec!["ext", "list"],
        vec!["ext", "show", "rust"],
        vec!["ext", "add", ext.to_str().unwrap()],
        vec!["ext", "remove", "hello"],
        vec!["trust"],
        vec!["consent", "kotlin-lsp"],
        vec!["trust", "--revoke"],
        vec!["unuse", "node"],
        vec!["show", "q:1"],
        vec!["problems"],
        vec!["find", "f"],
        vec!["info", "f"],
        vec!["refs", "f"],
        vec!["outline", "src/lib.rs"],
        vec!["install"],
        vec!["lock", "--refresh"],
        vec!["gc"],
        vec!["stop", "lsp:rust-analyzer"],
        vec!["cancel", "run:1"],
        vec!["env"],
        vec!["verbs"],
        vec!["stats"],
        vec!["daemon", "status"],
        vec!["hook", "post-edit"],
        vec!["rename", "x"],
    ];
    calls.push(vec!["status"]);
    for args in &calls {
        let (code, text, err) = run(&env, args);
        let want = match args[0] {
            "status" | "use" | "unuse" | "ext" | "trust" | "verbs" | "daemon" => Some(0),
            "consent" => Some(3),
            _ => code,
        };
        assert_eq!(code, want, "{args:?}: {text}{err}");
    }
    let mut mcp = env.mcp(&[]);
    mcp.handshake("t");
    for (tool, verb) in [("problems_symbols", "status"), ("test_build_run", "use"), ("problems_symbols", "ext"), ("test_build_run", "trust")] {
        mcp.call(tool, json!({"verb": verb, "args": if verb == "use" { json!(["bun"]) } else { json!([]) }}));
    }
    drop(mcp);
    let after = tree(&env.work());
    let changed: Vec<_> = after.iter().filter(|(p, m)| before.get(*p) != Some(m)).map(|(p, _)| p).chain(before.keys().filter(|p| !after.contains_key(*p))).collect();
    assert!(changed.is_empty(), "written inside the workspace: {changed:?}");
}
