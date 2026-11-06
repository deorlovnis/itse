use regex_lite::Regex;

use super::*;
use crate::sys::tests::TempDir;

#[test]
fn the_skill_is_short_has_front_matter_and_lists_every_tool_verb() {
    let text = skill("itse", "Workspace facts from itse: tests, problems");
    assert!(text.lines().count() <= 60, "{} lines:\n{text}", text.lines().count());
    assert!(text.starts_with("---\nname: \"itse\"\ndescription: \"Workspace facts from itse: tests, problems\"\n---\n"), "{text}");
    for v in VERBS.iter().filter(|v| matches!(v.tool, Tool::Run | Tool::Query | Tool::Show)) {
        assert!(text.contains(&format!("- `itse {}", v.name)), "{}: {text}", v.name);
    }
    assert!(text.contains("- `itse find <name> [--kind V]`: symbols by name\n") && !text.contains("itse trust") && !text.contains("itse integrate"), "{text}");
    assert!(text.contains("itse show run:14/T1") && text.contains("Exit codes: ") && text.contains("`itse verbs` lists this workspace's"), "{text}");
}

#[test]
fn the_skill_holds_no_imperatives() {
    // The same rule as the result templates': the skill states facts and leaves the choice to the agent.
    let imperative = Regex::new(r"(?i)please|you should|you must|make sure|don't|do not|always|never").unwrap();
    let text = skill("itse", "Workspace facts from itse: tests, problems, symbols by name");
    assert!(!imperative.is_match(&text), "{:?}", imperative.find(&text));
}

#[test]
fn a_merge_appends_once_keeps_every_other_key_and_removes_exactly() {
    let tmp = TempDir::new("itse-merge");
    let path = tmp.0.join("settings.json");
    let entry = json!({"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "itse hook post-edit", "timeout": 10}]});
    let theirs = json!({"model": "x", "hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": []}], "Stop": []}});
    fs::write(&path, theirs.to_string()).unwrap();
    let read = || serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap();
    let merge = |remove: bool| put(&path, "s", merged(read_json(&path)?.1, "s", "/hooks/PostToolUse", &entry, remove)?.as_ref().map(bytes), remove, false);
    assert!(merge(false).unwrap().starts_with("appended to "));
    assert_eq!(merge(false).unwrap(), "s: already there");
    assert_eq!(read()["hooks"]["PostToolUse"], json!([theirs["hooks"]["PostToolUse"][0], entry]));
    assert_eq!((&read()["model"], &read()["hooks"]["Stop"]), (&json!("x"), &json!([])));
    merge(true).unwrap();
    assert_eq!(read(), theirs, "removed exactly, and an empty array of theirs stays");
    // In a file that had none, the containers it made go with it.
    fs::write(&path, r#"{"model": "x"}"#).unwrap();
    merge(false).unwrap();
    merge(true).unwrap();
    assert_eq!(read(), json!({"model": "x"}));
    for shape in [r#"{"hooks": {"PostToolUse": {}}}"#, "null", "[]"] {
        fs::write(&path, shape).unwrap();
        assert!(merge(false).unwrap_err().to_string().ends_with("; not changed"), "{shape}");
        assert_eq!(fs::read_to_string(&path).unwrap(), shape);
    }
}

#[test]
fn a_merge_keeps_the_mode_of_the_file_it_replaces() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("itse-mode");
    let path = tmp.0.join("settings.json");
    fs::write(&path, "{}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    put(&path, "s", merged(json!({}), "s", "/hooks/PostToolUse", &json!({"a": 1}), false).unwrap().as_ref().map(bytes), false, false).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
}

#[test]
fn targets_outside_the_home_or_in_a_work_tree_are_refused() {
    let tmp = TempDir::new("itse-target");
    let home = fs::canonicalize(&tmp.0).unwrap();
    fs::create_dir_all(home.join("repo/.git")).unwrap();
    fs::create_dir_all(home.join("ws")).unwrap();
    assert_eq!(target(&home, "~/.claude/settings.json", &[]).unwrap(), home.join(".claude/settings.json"));
    for declared in ["/etc/x.json", "~/../x.json"] {
        assert!(target(&home, declared, &[]).unwrap_err().to_string().contains(" is outside "), "{declared}");
    }
    assert!(target(&home, "~/repo/.claude/settings.json", &[]).unwrap_err().to_string().contains(", a work tree or workspace; not changed"));
    assert!(target(&home, "~/ws/x.json", &[home.join("ws")]).is_err(), "a known workspace root");
    std::os::unix::fs::symlink(home.join("repo"), home.join(".claude")).unwrap();
    assert!(target(&home, "~/.claude/settings.json", &[]).is_err(), "a link into a repo");
}

#[test]
fn a_linked_target_is_the_file_it_names_and_a_dangling_link_is_refused() {
    let tmp = TempDir::new("itse-link");
    let home = fs::canonicalize(&tmp.0).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join("dotfiles")).unwrap();
    fs::write(home.join("dotfiles/settings.json"), "{}").unwrap();
    std::os::unix::fs::symlink(home.join("dotfiles/settings.json"), home.join(".claude/settings.json")).unwrap();
    assert_eq!(target(&home, "~/.claude/settings.json", &[]).unwrap(), home.join("dotfiles/settings.json"));
    std::os::unix::fs::symlink(home.join("dotfiles/missing.json"), home.join(".claude/other.json")).unwrap();
    assert!(target(&home, "~/.claude/other.json", &[]).unwrap_err().to_string().ends_with("other.json is a link to a missing file; not changed"));
}

#[test]
fn the_config_env_moves_the_declared_paths_into_its_folder() {
    let config = ConfigSpec { env: "CLAUDE_CONFIG_DIR".into(), paths: [("~/.claude".into(), String::new()), ("~/.claude.json".into(), ".claude.json".into())].into() };
    let moved = (&config, PathBuf::from("/cfg"));
    assert_eq!(place("~/.claude/skills/itse/SKILL.md", Some(&moved)), "/cfg/skills/itse/SKILL.md");
    assert_eq!((place("~/.claude", Some(&moved)), place("~/.claude.json", Some(&moved))), ("/cfg".into(), "/cfg/.claude.json".into()));
    assert_eq!(place("~/.claudex/a", Some(&moved)), "~/.claudex/a", "a prefix of a name is no match");
    assert_eq!(place("~/.claude/settings.json", None), "~/.claude/settings.json");
}

#[test]
fn a_skill_an_older_itse_wrote_is_noted_until_it_is_written_again() {
    let tmp = TempDir::new("itse-skill");
    let vars: BTreeMap<&str, std::ffi::OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    let dirs = Dirs::from_env(|k| vars.get(k).cloned()).unwrap();
    let exts = manifest::extensions(&dirs).unwrap().0;
    let (path, spec) = (tmp.0.join("SKILL.md"), serde_json::from_value::<Integration>(exts["claude-code"].0.integration.clone().unwrap()).unwrap().skill.unwrap());
    let stale = |text: &str, recorded: &str| {
        fs::write(&path, text).unwrap();
        record(&dirs, &format!("claude-code skill {}", path.display()), Some(json!({"sha256": sys::sha256_hex(recorded.as_bytes())}))).unwrap();
        stale_skills(&dirs, &exts)
    };
    let old = "---\nname: itse\n---\n- `itse debug test`: debug the tests\n";
    assert_eq!(stale(old, old), [format!("skill: {} is an older itse's; 'itse integrate claude-code --skill --yes' updates it", path.display())]);
    assert_eq!(stale(old, "edited since"), Vec::<String>::new(), "a skill changed since it was written is left alone");
    let now = skill(&spec.name, &spec.description);
    assert_eq!(stale(&now, &now), Vec::<String>::new());
}
