//! `itse integrate claude-code` and `itse hook post-edit` end to end: the skill, the hook recipe's
//! merge and its removal, the refusals, MCP registration through a fake `claude` on PATH, and the
//! hook against the fixture LSP, which never starts the daemon or a server.

use std::os::unix::fs::PermissionsExt;

use super::lsp::{Fix, LIB, symbols};
use super::*;

/// `itse ARGS` in `cwd` with this PATH: (exit code, stdout and stderr).
fn run(env: &Env, cwd: &Path, path: &str, args: &[&str]) -> (Option<i32>, String) {
    let out = env.command(cwd, args).env("PATH", path).output().unwrap();
    (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
}

/// A `claude` that logs its cwd, HOME, CLAUDE_CONFIG_DIR and arguments, one per line and `--` after
/// each call, and keeps its local-scope servers, answering as the real one does to a second add or
/// a removal of one it doesn't have.
fn fake_claude(env: &Env) -> (String, PathBuf) {
    let (bin, log, state) = (env.dir.join("bin"), env.dir.join("claude.log"), env.dir.join("claude.servers"));
    fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$PWD" "$HOME" "${{CLAUDE_CONFIG_DIR-unset}}" "$@" -- >>'{log}'
entry="$PWD $3"
touch '{state}'
case "$2" in
add-json)
    if grep -qxF "$entry" '{state}'; then echo "MCP server $3 already exists in local config" >&2; exit 1; fi
    echo "$entry" >>'{state}'
    echo "Added stdio MCP server $3 to local config";;
remove)
    if ! grep -qxF "$entry" '{state}'; then echo "No MCP server named \"$3\" in local scope" >&2; exit 1; fi
    grep -vxF "$entry" '{state}' >'{state}.new'; mv '{state}.new' '{state}'
    echo "Removed MCP server $3 from local config";;
esac
"#,
        log = log.display(),
        state = state.display()
    );
    fs::write(bin.join("claude"), script).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    (format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()), log)
}

/// The fake's calls, each as its lines: cwd, HOME, CLAUDE_CONFIG_DIR, then the arguments.
fn calls(log: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(log).unwrap_or_default();
    text.split_terminator("--\n").map(|call| call.lines().map(String::from).collect()).collect()
}

#[test]
fn integrate_mcp_runs_claude_mcp_add_json_at_local_scope() {
    let env = Env::new();
    let (path, log) = fake_claude(&env);
    let (work, home) = (env.work(), fs::canonicalize(env.dir.join("home")).unwrap());
    let line = r#"claude mcp add-json itse '{"args":["mcp"],"command":"itse","type":"stdio"}' --scope local"#;
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("OK integrate claude-code — 1 planned\n"), "{text}");
    assert!(text.contains(&format!("\nwould run in {} (HOME {}): {line}\n", work.display(), home.display())), "{text}");
    assert!(text.contains("next (human): itse integrate claude-code --mcp --yes in a terminal"), "{text}");
    assert!(!log.exists(), "nothing runs without --yes");
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--yes"]);
    assert_eq!((code, text.lines().next()), (Some(0), Some("OK integrate claude-code — 1 done by non-tty")), "{text}");
    assert!(text.contains(&format!("\nran {line}: Added stdio MCP server itse to local config\n")), "{text}");
    // Again: claude says it is there, and itse added it, so that is done.
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--yes"]);
    assert!(code == Some(0) && text.contains(&format!("\nran {line}: MCP server itse already exists in local config; there already\n")), "{text}");
    // A human removed it with claude's own command since: it is added again.
    let (w, h) = (work.display().to_string(), home.display().to_string());
    assert!(Command::new(env.dir.join("bin/claude")).args(["mcp", "remove", "itse"]).current_dir(&work).env("HOME", &home).output().unwrap().status.success());
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--yes"]);
    assert!(code == Some(0) && text.contains(&format!("\nran {line}: Added stdio MCP server itse to local config\n")), "{text}");
    assert_eq!(fs::read_to_string(env.dir.join("claude.servers")).unwrap(), format!("{w} itse\n"));
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains(": Removed MCP server itse from local config\n"), "{text}");
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains(": itse never added it\n"), "a second removal asks claude nothing: {text}");
    let added = [&w, &h, "unset", "mcp", "add-json", "itse", r#"{"args":["mcp"],"command":"itse","type":"stdio"}"#, "--scope", "local"];
    let removed = [&w, &h, "unset", "mcp", "remove", "itse", "--scope", "local"];
    let by_hand = [&w, &h, "unset", "mcp", "remove", "itse"];
    assert_eq!(calls(&log), [&added[..], &added[..], &by_hand[..], &added[..], &removed[..]], "in the workspace root, with the JSON as one argument");
    assert!(!home.join(".claude.json").exists() && !work.join(".claude.json").exists(), "itse never writes claude's own config");
    let record: Value = serde_json::from_slice(&fs::read(env.dir.join("state/itse/integrations.json")).unwrap()).unwrap();
    assert_eq!(record, json!({}), "the removal took the record back too");
    // A server the user added by hand is theirs: an add fails as claude says, and a removal keeps it.
    assert!(Command::new(env.dir.join("bin/claude")).args(["mcp", "add-json", "itse", "{}"]).current_dir(&work).output().unwrap().status.success());
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--yes"]);
    assert_eq!(code, Some(1), "{text}");
    assert!(
        text.starts_with("FAIL integrate claude-code — 0 done by non-tty, 1 failed\n") && text.ends_with(": exited 1: MCP server itse already exists in local config\n"),
        "{text}"
    );
    assert_eq!(text.matches(line).count(), 1, "the command line once: {text}");
    assert!(run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--remove", "--yes"]).1.contains(": itse never added it\n"));
    assert_eq!(fs::read_to_string(env.dir.join("claude.servers")).unwrap(), format!("{w} itse\n"), "kept");
    // One itse added that the user removed since is gone already: the removal is done.
    fs::write(env.dir.join("claude.servers"), "").unwrap();
    fs::write(env.dir.join("state/itse/integrations.json"), json!({format!("claude-code mcp {w}"): {"actor": "tty"}}).to_string()).unwrap();
    let (code, text) = run(&env, &work, &path, &["integrate", "claude-code", "--mcp", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains("No MCP server named \"itse\" in local scope; gone already\n"), "{text}");
    // Without claude on PATH, the human gets the command to run.
    let (code, text) = run(&env, &work, "/usr/bin:/bin", &["integrate", "claude-code", "--mcp", "--yes"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("NEEDS_HUMAN integrate claude-code — 0 done by non-tty\n"), "{text}");
    assert!(text.contains(&format!("claude is not on PATH; a human runs, in {w}: {line}")), "{text}");
}

#[test]
fn integrate_writes_where_claude_config_dir_says_and_home_drops_it() {
    let env = Env::new();
    let (path, log) = fake_claude(&env);
    let (work, home) = (env.work(), fs::canonicalize(env.dir.join("home")).unwrap());
    let config = home.join("cc");
    fs::create_dir_all(&config).unwrap();
    let itse = |cwd: &Path, args: &[&str], config: &Path| stdout(&env.command(cwd, args).env("PATH", &path).env("CLAUDE_CONFIG_DIR", config).output().unwrap());
    let all = ["integrate", "claude-code", "--skill", "--mcp", "--recipe", "post-edit", "--yes"];
    let text = itse(&work, &all, &config);
    assert!(text.starts_with("OK integrate claude-code — 3 done by non-tty\n"), "{text}");
    assert!(config.join("skills/itse/SKILL.md").exists() && config.join("settings.json").exists() && !home.join(".claude").exists(), "{text}");
    assert_eq!(calls(&log)[0][..3], [work.display().to_string(), home.display().to_string(), config.display().to_string()]);
    let text = itse(&work, &["integrate", "claude-code", "--mcp", "--remove"], &config);
    assert!(text.contains(&format!(" (HOME {}, CLAUDE_CONFIG_DIR {}): claude mcp remove ", home.display(), config.display())), "{text}");
    // With --home, ~ at DIR decides for claude too.
    let other = home.join("other");
    fs::create_dir_all(&other).unwrap();
    let text = itse(&home, &["integrate", "claude-code", "--skill", "--mcp", "--home", other.to_str().unwrap(), "--yes"], &config);
    assert!(text.starts_with("OK ") && other.join(".claude/skills/itse/SKILL.md").exists(), "{text}");
    assert_eq!(calls(&log).last().unwrap()[1..3], [other.display().to_string(), "unset".to_string()]);
    // A config folder in a repo is refused like any target there.
    fs::create_dir_all(home.join("repo/.git")).unwrap();
    let text = itse(&work, &all, &home.join("repo/cc"));
    assert!(text.contains(", a work tree or workspace; not changed"), "{text}");
}

#[test]
fn the_hook_recipe_is_added_once_and_removed_exactly() {
    let env = Env::new();
    let settings = env.dir.join("home/.claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let theirs =
        json!({"model": "opus", "permissions": {"allow": ["Bash(ls)"]}, "hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "true"}]}]}});
    // Their own layout: keys unsorted, four spaces.
    let layout = "{\n    \"permissions\": {\"allow\": [\"Bash(ls)\"]},\n    \"model\": \"opus\",\n    \"hooks\": {\"PostToolUse\": [{\"matcher\": \"Bash\", \"hooks\": [{\"type\": \"command\", \"command\": \"true\"}]}]}\n}\n";
    fs::write(&settings, layout).unwrap();
    let read = || serde_json::from_slice::<Value>(&fs::read(&settings).unwrap()).unwrap();
    let itse = |args: &[&str]| run(&env, &env.work(), &std::env::var("PATH").unwrap(), args);
    let (code, text) = itse(&["integrate", "claude-code", "--recipe", "post-edit"]);
    assert_eq!(code, Some(0), "{text}");
    let canonical = fs::canonicalize(&settings).unwrap();
    assert!(text.contains(&format!("\nwould append to {} at /hooks/PostToolUse: {{\"hooks\":[{{\"command\":\"itse hook post-edit\"", canonical.display())), "{text}");
    assert!(text.contains("\"} (the file is written with its keys sorted)\n"), "the dry run says the layout changes: {text}");
    assert_eq!(read(), theirs, "nothing changes without --yes");
    let entry = json!({"matcher": "Edit|Write|MultiEdit", "hooks": [{"type": "command", "command": "itse hook post-edit", "timeout": 10}]});
    for _ in 0..2 {
        assert_eq!(itse(&["integrate", "claude-code", "--recipe", "post-edit", "--yes"]).0, Some(0));
    }
    let (_, text) = itse(&["integrate", "claude-code", "--recipe", "post-edit", "--yes"]);
    assert!(text.contains(" at /hooks/PostToolUse: already there\n"), "{text}");
    assert_eq!(read()["hooks"]["PostToolUse"], json!([theirs["hooks"]["PostToolUse"][0], entry]), "added once, after theirs");
    assert_eq!((&read()["model"], &read()["permissions"]), (&theirs["model"], &theirs["permissions"]), "every other key stays");
    let (code, text) = itse(&["integrate", "claude-code", "--recipe", "post-edit", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains(&format!("\nrestored {} as it was\n", canonical.display())), "{text}");
    assert_eq!(fs::read_to_string(&settings).unwrap(), layout, "unchanged since itse wrote it, so back byte for byte");
    // Changed since, by the user or the harness: only the entry goes, and every other key stays.
    assert_eq!(itse(&["integrate", "claude-code", "--recipe", "post-edit", "--yes"]).0, Some(0));
    let mut edited = read();
    edited["theme"] = json!("dark");
    fs::write(&settings, edited.to_string()).unwrap();
    let (code, text) = itse(&["integrate", "claude-code", "--recipe", "post-edit", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains(&format!("\nremoved from {} at /hooks/PostToolUse\n", canonical.display())), "{text}");
    edited["hooks"] = theirs["hooks"].clone();
    assert_eq!(read(), edited, "removed exactly");
    // A settings file that didn't exist is made, and goes again with the removal, even edited since.
    for edit in [false, true] {
        let _ = fs::remove_file(&settings);
        assert_eq!(itse(&["integrate", "claude-code", "--recipe", "post-edit", "--yes"]).0, Some(0));
        assert_eq!(read(), json!({"hooks": {"PostToolUse": [entry]}}));
        if edit {
            fs::write(&settings, read().to_string()).unwrap();
        }
        let (code, text) = itse(&["integrate", "claude-code", "--recipe", "post-edit", "--remove", "--yes"]);
        assert_eq!(code, Some(0), "{text}");
        assert!(!settings.exists(), "{text}");
    }
}

#[test]
fn integrate_refuses_a_target_inside_a_work_tree() {
    let env = Env::new();
    let work = env.work();
    let (path, log) = fake_claude(&env);
    let itse = |cwd: &Path, args: &[&str]| run(&env, cwd, &path, args);
    // The workspace itself, git or not, is never a harness's home.
    let (code, text) = itse(&work, &["integrate", "claude-code", "--skill", "--home", work.to_str().unwrap(), "--yes"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains(&format!("{}/.claude/skills/itse/SKILL.md is inside {}, a work tree or workspace; not changed", work.display(), work.display())), "{text}");
    let repo = env.dir.join("home/repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let (code, text) = itse(&work, &["integrate", "claude-code", "--skill", "--recipe", "post-edit", "--home", repo.to_str().unwrap(), "--yes"]);
    assert!(code == Some(2) && text.contains(", a work tree or workspace; not changed"), "{text}");
    // Nor where claude would write its own config.
    for home in [&work, &repo] {
        let (code, text) = itse(&work, &["integrate", "claude-code", "--mcp", "--home", home.to_str().unwrap(), "--yes"]);
        assert!(code == Some(2) && text.contains("/.claude.json is inside ") && text.contains(", a work tree or workspace; not changed"), "{text}");
    }
    assert!(!log.exists(), "claude never ran");
    // A workspace without git that had a daemon is one too.
    let proj = env.dir.join("home/proj");
    fs::create_dir_all(&proj).unwrap();
    assert_eq!(itse(&proj, &["status"]).0, Some(0));
    let (code, text) = itse(&work, &["integrate", "claude-code", "--skill", "--home", proj.to_str().unwrap(), "--yes"]);
    assert!(code == Some(2) && text.contains(", a work tree or workspace; not changed"), "{text}");
    assert!(!work.join(".claude").exists() && !repo.join(".claude").exists() && !proj.join(".claude").exists(), "nothing was written");
}

#[test]
fn integrate_runs_from_the_home_folder() {
    let env = Env::new();
    let home = env.dir.join("home");
    let itse = |cwd: &Path, args: &[&str]| run(&env, cwd, &std::env::var("PATH").unwrap(), args);
    // `~` has no git, so it is the workspace of a call there and had a daemon; neither refuses ~/.claude.
    assert_eq!(itse(&home, &["status"]).0, Some(0));
    let (code, text) = itse(&home, &["integrate", "claude-code", "--skill", "--recipe", "post-edit", "--yes"]);
    assert_eq!(code, Some(0), "{text}");
    let (code, text) = itse(&env.work(), &["integrate", "claude-code", "--skill", "--recipe", "post-edit", "--remove", "--yes"]);
    assert!(code == Some(0) && !home.join(".claude/skills/itse").exists(), "{text}");
}

#[test]
fn integrate_follows_a_linked_settings_file_and_keeps_its_mode() {
    let env = Env::new();
    let (settings, real) = (env.dir.join("home/.claude/settings.json"), env.dir.join("home/dotfiles/settings.json"));
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, r#"{"model": "opus"}"#).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&real, &settings).unwrap();
    let (code, text) = run(&env, &env.work(), &std::env::var("PATH").unwrap(), &["integrate", "claude-code", "--recipe", "post-edit", "--yes"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(fs::symlink_metadata(&settings).unwrap().file_type().is_symlink(), "the link stays");
    let doc: Value = serde_json::from_slice(&fs::read(&real).unwrap()).unwrap();
    assert!(doc["model"] == "opus" && doc["hooks"]["PostToolUse"].is_array(), "{doc}");
    assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o644);
}

#[test]
fn integrate_leaves_invalid_json_untouched() {
    let env = Env::new();
    let settings = env.dir.join("home/.claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, "{\"model\": \"opus\",\n").unwrap();
    let (code, text) = run(&env, &env.work(), &std::env::var("PATH").unwrap(), &["integrate", "claude-code", "--skill", "--recipe", "post-edit", "--yes"]);
    assert_eq!(code, Some(2), "{text}");
    assert_eq!(text, format!("ERROR integrate — {} is not valid JSON; not changed\n", fs::canonicalize(&settings).unwrap().display()));
    assert_eq!(fs::read_to_string(&settings).unwrap(), "{\"model\": \"opus\",\n");
    assert!(!env.dir.join("home/.claude/skills").exists(), "the skill isn't written either: nothing changes");
    // Valid JSON of another shape refuses the call the same way.
    fs::write(&settings, r#"{"hooks": {"PostToolUse": {}}}"#).unwrap();
    let (code, text) = run(&env, &env.work(), &std::env::var("PATH").unwrap(), &["integrate", "claude-code", "--skill", "--recipe", "post-edit", "--yes"]);
    assert!(code == Some(2) && text.ends_with(" at /hooks/PostToolUse: not an array; not changed\n"), "{text}");
    assert!(!env.dir.join("home/.claude/skills").exists() && !env.dir.join("state/itse/integrations.json").exists());
}

#[test]
fn the_skill_is_the_same_in_every_workspace() {
    let env = Env::new();
    let other = env.dir.join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    let itse = |cwd: &Path, args: &[&str]| run(&env, cwd, &std::env::var("PATH").unwrap(), args);
    assert_eq!(itse(&other, &["use", "rust"]).0, Some(0));
    let skill = env.dir.join("home/.claude/skills/itse/SKILL.md");
    let mut seen = Vec::new();
    for (cwd, home) in [(env.work(), env.dir.join("home")), (other.clone(), env.dir.join("home2"))] {
        fs::create_dir_all(&home).unwrap();
        let (code, text) = itse(&cwd, &["integrate", "claude-code", "--skill", "--home", home.to_str().unwrap(), "--yes"]);
        assert_eq!(code, Some(0), "{text}");
        seen.push(fs::read_to_string(home.join(".claude/skills/itse/SKILL.md")).unwrap());
    }
    assert_eq!(seen[0], seen[1], "the same file wherever it is made");
    assert!(seen[0].lines().count() <= 60, "{} lines", seen[0].lines().count());
    assert!(
        seen[0].starts_with("---\nname: \"itse\"\ndescription: \"Workspace facts from itse: ") && seen[0].contains("\n- `itse test [target] [filter] [--action V]`: "),
        "{}",
        seen[0]
    );
    let record: Value = serde_json::from_slice(&fs::read(env.dir.join("state/itse/integrations.json")).unwrap()).unwrap();
    let key = format!("claude-code skill {}", fs::canonicalize(&skill).unwrap().display());
    assert_eq!(record[&key]["actor"], "non-tty", "{record}");
    // --remove takes back what it wrote, its folder and the skills folder too if it made them, each once empty; a
    // skill changed since is kept.
    let theirs = env.dir.join("home2/.claude/skills/theirs");
    fs::create_dir_all(&theirs).unwrap();
    let (code, text) = itse(&env.work(), &["integrate", "claude-code", "--skill", "--remove", "--yes"]);
    assert!(code == Some(0) && text.contains("\nremoved "), "{text}");
    assert!(!skill.exists() && !env.dir.join("home/.claude/skills").exists() && env.dir.join("home/.claude").exists());
    let changed = env.dir.join("home2/.claude/skills/itse/SKILL.md");
    fs::write(&changed, "mine\n").unwrap();
    let (code, text) = itse(&other, &["integrate", "claude-code", "--skill", "--remove", "--home", env.dir.join("home2").to_str().unwrap(), "--yes"]);
    assert!(code == Some(1) && text.contains(" changed since itse wrote it; not removed"), "{text}");
    assert_eq!(fs::read_to_string(&changed).unwrap(), "mine\n");
    fs::write(&changed, &seen[0]).unwrap();
    let (code, text) = itse(&other, &["integrate", "claude-code", "--skill", "--remove", "--home", env.dir.join("home2").to_str().unwrap(), "--yes"]);
    assert!(code == Some(0) && !changed.exists() && theirs.exists(), "a skills folder with others in it stays: {text}");
}

#[test]
fn a_skill_removed_takes_only_the_folders_itse_made() {
    let env = Env::new();
    let itse = |args: &[&str]| run(&env, &env.work(), &std::env::var("PATH").unwrap(), args);
    let (skills, skill) = (env.dir.join("home/.claude/skills"), env.dir.join("home/.claude/skills/itse/SKILL.md"));
    fs::create_dir_all(skills.join("itse")).unwrap();
    for _ in 0..2 {
        let (code, text) = itse(&["integrate", "claude-code", "--skill", "--yes"]);
        assert_eq!(code, Some(0), "{text}");
    }
    let (code, text) = itse(&["integrate", "claude-code", "--skill", "--remove", "--yes"]);
    assert!(code == Some(0) && !skill.exists() && skills.join("itse").is_dir(), "the folders were there before: {text}");
    fs::remove_dir_all(&skills).unwrap();
    for _ in 0..2 {
        assert_eq!(itse(&["integrate", "claude-code", "--skill", "--yes"]).0, Some(0));
    }
    let record: Value = serde_json::from_slice(&fs::read(env.dir.join("state/itse/integrations.json")).unwrap()).unwrap();
    let made = &record[format!("claude-code skill {}", fs::canonicalize(&skill).unwrap().display())]["made"];
    assert_eq!(made.as_array().map(Vec::len), Some(2), "a second write keeps what the first made: {record}");
    let (code, text) = itse(&["integrate", "claude-code", "--skill", "--remove", "--yes"]);
    assert!(code == Some(0) && !skills.exists() && env.dir.join("home/.claude").is_dir(), "{text}");
}

// --- the post-edit hook ---

/// `itse hook post-edit` in `cwd` with this stdin: (exit code, stdout, stderr).
fn hook(fix: &Fix, cwd: &Path, stdin: &Value, extra: &[(&str, &str)]) -> (Option<i32>, String, String) {
    hook_raw(fix, cwd, stdin.to_string().as_bytes(), extra)
}

fn hook_raw(fix: &Fix, cwd: &Path, stdin: &[u8], extra: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let mut child = fix.env.command(cwd, &["hook", "post-edit"]).envs(extra.iter().copied()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // A hook that stops reading early may close its end first.
    let _ = child.stdin.take().unwrap().write_all(stdin);
    let out = child.wait_with_output().unwrap();
    (out.status.code(), stdout(&out), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// What Claude Code sends after an edit of `file`, from `cwd`, in session `session`.
fn edited(file: &str, cwd: &Path, session: &str) -> Value {
    json!({"session_id": session, "transcript_path": "/tmp/t.jsonl", "cwd": cwd, "hook_event_name": "PostToolUse", "tool_name": "Edit",
        "tool_input": {"file_path": file, "old_string": "a", "new_string": "b"}, "tool_response": {"success": true}})
}

/// The `additionalContext` of a hook's output, which holds nothing else.
fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["hookSpecificOutput"], "no decision of any kind: {v}");
    let inner: Vec<&String> = v["hookSpecificOutput"].as_object().unwrap().keys().collect();
    assert_eq!(inner, ["additionalContext", "hookEventName"], "no permission or classifier field: {v}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
}

#[test]
fn hook_never_starts_the_daemon() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    assert!(fix.itse(&["daemon", "stop"]).1.starts_with("stopped daemon pid "));
    fs::write(work.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let lib = work.join("src/lib.rs");
    assert_eq!(hook(&fix, &work, &edited(lib.to_str().unwrap(), &work, "s1"), &[]), (Some(0), String::new(), String::new()));
    assert!(fix.env.daemon_json().is_null() && !fix.env.socket().exists(), "no daemon started");
    // Nor for a payload that isn't an edit, isn't JSON, is empty or is over 1 MB.
    let mut big = edited(lib.to_str().unwrap(), &work, "s1").to_string().into_bytes();
    big.splice(1..1, format!("\"pad\": \"{}\", ", "x".repeat(1 << 20)).into_bytes());
    for stdin in [&json!("not an edit").to_string().into_bytes(), &b"{"[..].to_vec(), &vec![], &big] {
        assert_eq!(hook_raw(&fix, &work, stdin, &[]), (Some(0), String::new(), String::new()), "{} bytes", stdin.len());
    }
    assert!(fix.env.daemon_json().is_null() && !fix.env.socket().exists());
    // The same payload under 1 MB is an edit the running daemon answers.
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    let fits = String::from_utf8(big).unwrap().replace(&"x".repeat(1 << 20), "x");
    assert!(context(&hook_raw(&fix, &work, fits.as_bytes(), &[]).1).starts_with("FAIL problems src/lib.rs — 1 new error"));
}

#[test]
fn hook_never_starts_a_server() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    fs::write(work.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let edit = edited(work.join("src/lib.rs").to_str().unwrap(), &work, "s1");
    assert_eq!(fix.itse(&["status"]).0, Some(0), "a daemon runs, with no server");
    assert_eq!(hook(&fix, &work, &edit, &[]), (Some(0), String::new(), String::new()));
    assert!(fix.env.usage().iter().any(|r| r["via"] == "hook" && r["verb"] == "problems"), "the daemon was asked");
    assert!(!fix.log.exists(), "the fixture server never started");
    // Once something else started it, the hook asks it, with the edited file opened.
    assert_eq!(fix.itse(&["problems"]).0, Some(0), "a fresh server has opened no file");
    let (code, out, _) = hook(&fix, &work, &edit, &[]);
    assert_eq!(code, Some(0));
    let text = context(&out);
    assert!(text.starts_with("FAIL problems src/lib.rs — 1 new error, 0 new warnings; 0 unchanged, 0 fixed since ") && text.contains("; fixture index warm; q:"), "{text}");
}

#[test]
fn hook_takes_the_workspace_from_the_edited_file() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    fs::write(work.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    // The hook runs elsewhere, as a harness's hook may; the relative path is the payload cwd's.
    let elsewhere = fix.env.dir.clone();
    let (code, out, _) = hook(&fix, &elsewhere, &edited("src/lib.rs", &work, "s1"), &[]);
    assert_eq!(code, Some(0));
    let text = context(&out);
    assert!(text.starts_with("FAIL problems src/lib.rs — 1 new error") && text.contains("\nE1 error src/lib.rs:3:1 FIXTURE_ERROR found [fixture]\n"), "{text}");
    // A session whose cwd is another workspace still asks the edited file's.
    let other = fix.env.dir.join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    fs::write(other.join("lib.rs"), "FIXTURE_ERROR\n").unwrap();
    assert_eq!(fix.env.command(&other, &["use", "fix"]).output().unwrap().status.code(), Some(0));
    let (_, out, _) = hook(&fix, &elsewhere, &edited(work.join("src/lib.rs").to_str().unwrap(), &other, "s2"), &[]);
    assert!(context(&out).starts_with("FAIL problems src/lib.rs — 1 new error"), "{out}");
    // And an edit in that other workspace, which has no daemon, says nothing and asks no daemon.
    let asked = fix.env.usage().iter().filter(|r| r["via"] == "hook").count();
    assert_eq!(hook(&fix, &work, &edited("../other/lib.rs", &work, "s1"), &[]), (Some(0), String::new(), String::new()));
    assert_eq!(fix.env.usage().iter().filter(|r| r["via"] == "hook").count(), asked);
    // An edit outside any workspace, here the harness's own config, answers at once with nothing.
    let config = fix.env.dir.join("home/.claude/settings.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "{}").unwrap();
    let started = Instant::now();
    assert_eq!(hook(&fix, &work, &edited(config.to_str().unwrap(), &work, "s1"), &[]), (Some(0), String::new(), String::new()));
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

#[test]
fn hook_finds_a_workspace_without_git_or_rooted_below_its_git_root() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let (nogit, sub) = (fix.env.dir.join("nogit"), fix.env.work().join("sub"));
    for (root, given) in [(&nogit, None), (&sub, Some(&sub))] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), LIB).unwrap();
        let itse = |args: &[&str]| {
            let mut cmd = fix.env.command(root, args);
            cmd.env("ITSE_SERVER_IDLE_S", "60").envs(given.map(|g| ("ITSE_WORKSPACE", g)));
            cmd.output().unwrap()
        };
        for args in [&["use", "fix"][..], &["problems"]] {
            assert!(itse(args).status.success(), "{args:?}: {}", stdout(&itse(args)));
        }
        fs::write(root.join("src/lib.rs"), format!("{LIB}FIXTURE_ERROR\n")).unwrap();
        // From src, where a walk up to git would end at the wrong root.
        let (code, out, _) = hook(&fix, &root.join("src"), &edited("lib.rs", &root.join("src"), "s1"), &[]);
        assert_eq!(code, Some(0));
        assert!(context(&out).starts_with("FAIL problems src/lib.rs — 1 new error"), "{}: {out}", root.display());
    }
}

#[test]
fn hook_prints_nothing_when_no_new_errors() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    let lib = work.join("src/lib.rs");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    let edit = edited(lib.to_str().unwrap(), &work, "s1");
    fs::write(&lib, format!("{LIB}// a comment\n")).unwrap();
    assert_eq!(hook(&fix, &work, &edit, &[]).1, "", "clean");
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    assert!(context(&hook(&fix, &work, &edit, &[]).1).starts_with("FAIL problems src/lib.rs — 1 new error"));
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n// more\n")).unwrap();
    assert_eq!(hook(&fix, &work, &edit, &[]).1, "", "the same error isn't new");
}

#[test]
fn an_unsettled_hook_call_prints_nothing_and_keeps_the_baseline() {
    let Some(fix) = Fix::new(json!({"symbols": symbols(), "index_ms": 5000}), "", false) else { return };
    let (work, started) = (fix.env.work(), Instant::now());
    let fast = [("ITSE_TEST_SETTLE_MS", "500")];
    assert_eq!(fix.itse_env(&["problems"], &fast).0, Some(0), "starts the server, still indexing");
    let lib = work.join("src/lib.rs");
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let edit = edited(lib.to_str().unwrap(), &work, "s1");
    assert_eq!(hook(&fix, &work, &edit, &fast), (Some(0), String::new(), String::new()), "busy indexing: not settled");
    let records = fix.env.usage();
    let last = records.iter().rfind(|r| r["via"] == "hook").unwrap();
    assert_eq!((&last["verb"], &last["settled"], &last["warm"]), (&json!("problems"), &json!(false), &json!(false)), "{last}");
    let baselines = fs::read_to_string(fix.env.daemons()[0].0.join("baselines.json")).unwrap_or_default();
    assert!(!baselines.contains("\"hook:"), "not advanced: {baselines}");
    thread::sleep(Duration::from_millis(5200).saturating_sub(started.elapsed()));
    let text = context(&hook(&fix, &work, &edit, &[]).1);
    assert!(text.starts_with("FAIL problems src/lib.rs — 1 new error, 0 new warnings; 0 unchanged, 0 fixed; fixture index warm; q:"), "still new once settled: {text}");
    assert_eq!(fix.env.usage().iter().rfind(|r| r["via"] == "hook").unwrap()["settled"], true);
    assert_eq!(hook(&fix, &work, &edit, &[]).1, "");
}

#[test]
fn two_hook_sessions_keep_separate_baselines_through_the_cli() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    let lib = work.join("src/lib.rs");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    fs::write(&lib, format!("{LIB}FIXTURE_ERROR\n")).unwrap();
    let call = |session: &str| hook(&fix, &work, &edited(lib.to_str().unwrap(), &work, session), &[]).1;
    assert!(context(&call("session-a")).starts_with("FAIL problems src/lib.rs — 1 new error"));
    assert_eq!(call("session-a"), "");
    assert!(context(&call("session-b")).starts_with("FAIL problems src/lib.rs — 1 new error"), "another session has its own baseline");
    let baselines = fs::read_to_string(fix.env.daemons()[0].0.join("baselines.json")).unwrap();
    assert!(!baselines.contains("session-a") && baselines.matches("\"hook:").count() == 2, "{baselines}");
}

#[test]
fn hook_output_is_at_most_2000_chars() {
    let Some(fix) = Fix::new(json!({"symbols": symbols()}), "", false) else { return };
    let work = fix.env.work();
    let lib = work.join("src/lib.rs");
    assert_eq!(fix.itse(&["problems"]).0, Some(0));
    fs::write(&lib, format!("{LIB}{}", "let x = FIXTURE_ERROR; // a line long enough to fill the page quickly, again and again\n".repeat(120))).unwrap();
    let (code, out, _) = hook(&fix, &work, &edited(lib.to_str().unwrap(), &work, "s1"), &[]);
    let text = context(&out);
    assert_eq!(code, Some(0));
    assert!(text.chars().count() <= 2000, "{} chars", text.chars().count());
    assert!(text.starts_with("FAIL problems src/lib.rs — 120 new errors, 0 new warnings;") && text.contains("; full: itse show q:"), "the rest is one show away: {text}");
}
