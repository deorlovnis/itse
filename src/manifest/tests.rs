use super::*;

/// A full extension and a task entry, as an author would write them.
const KOTLIN: &str = r#"{ "itse_ext": 1, "id": "kotlin", "version": "0.1.0", "requires": ["jdk"], "detect": ["settings.gradle.kts", "build.gradle.kts"],
  "packages": { "kotlin-lsp": {
  "purl": "pkg:generic/jetbrains/kotlin-lsp", "install": "fetch",
  "pin_source": { "kind": "openvsx-bundle", "extension": "JetBrains/kotlin-server",
                  "platforms": { "linux_arm64": "linux-arm64", "linux_x64": "linux-x64" } },
  "archive": "tar.gz", "strip": 1, "bin": { "kotlin-lsp": "bin/intellij-server" },
  "hosts": ["open-vsx.org", "download.jetbrains.com"],
  "expires_after_days": 30, "exit_codes": { "7": "server_expired", "11": "needs_consent:eula" },
  "consents": [
    { "id": "eula", "kind": "hash", "of_file": "EULA.txt", "hex_prefix": 16, "flag": "--eula=" },
    { "id": "region", "kind": "choice", "env": "INTELLIJ_REGION" },
    { "id": "data_sharing", "kind": "choice", "env": "INTELLIJ_DATA_SHARING", "values": ["none","anonymous","full"] } ],
  "jvm_args": ["-Xmx2g"], "jvm_args_env": "IJ_JAVA_OPTIONS", "executes_project_code": true } },
  "servers": [
{ "id": "kotlin-lsp", "protocol": "lsp", "transport": "stdio",
  "command": ["${pkg:kotlin-lsp}/bin/intellij-server", "--stdio", "--system-path=${cache}/kotlin-lsp", "${consent:eula}"],
  "filetypes": ["kt","kts"], "languages": { "kt": "kotlin", "kts": "kotlin" },
  "env": { "JAVA_HOME": "${pkg:jdk}" }, "init_options": { "defaultSdk": "${pkg:jdk}", "buildTools": ["gradle"] } },
{ "id": "kotlin-dap", "protocol": "dap", "adapter_id": "intellij_debugger",
  "via": { "lsp_command": { "server": "kotlin-lsp", "command": "start_debug_server" } },
  "hide_vars": ["^\\$i\\$a\\$"] } ],
  "problemMatchers": { "kotlinc": { "owner": "kotlinc", "fileLocation": "absolute",
  "pattern": { "regexp": "^([ew]): file://([^:]+):(\\d+):(\\d+) (.*)$", "severity": 1, "file": 2, "line": 3, "column": 4, "message": 5 } } },
  "skills": [ { "name": "kotlin-module-scoped", "text": "Gradle runs per module: itse test :data." } ] }"#;

const GRADLE_TEST: &str = r#"{ "label": "gradle:test", "type": "process", "command": "${workspaceFolder}/gradlew", "group": {"kind":"test","isDefault":true},
  "args": ["${input:target}:test", "--console=plain", "--continue", "${filter_args}"], "filter_args": ["--tests", "${input:filter}"],
  "parse": ["junit-xml:**/build/test-results/**/TEST-*.xml"], "problemMatcher": ["$kotlinc"],
  "frames": { "regexp": "^\\s+at ([\\w.$]+)\\.([^.(]+)\\(([^:()]+):(\\d+)\\)$", "file": 3, "line": 4,
          "collapse": ["^(java|jdk|sun|kotlin\\.coroutines|org\\.junit|org\\.gradle|org\\.springframework)\\."] },
  "classify": [ { "regexp": "Could not find a valid Docker environment", "class": "environment", "note": "docker unavailable" } ],
  "outputs": ["build/", ".gradle/", "*/build/"], "executes_project_code": true }"#;

const LAYER: &str = r#"{ "itse": 1, "repo": "/work/crm/.git",
  "use": { "jdk": { "version": "25.0.5+8" }, "kotlin": {}, "gradle": {}, "node": { "install": "path" } },
  "system": { "libheif": { "lib": "libheif.so.1" }, "docker": { "cmd": ["docker","info"] } },
  "servers": { "kotlin-lsp": { "idle_minutes": 60 }, "some-mcp": { "expose": true, "tools": ["search", "fetch"], "max_list_bytes": 900 } },
  "inputs": [ { "id": "target", "type": "promptString" }, { "id": "filter", "type": "promptString" } ] }"#;

const CLAUDE_CODE: &str = r#"{ "itse_ext": 1, "id": "claude-code",
  "identity": { "session_env": "CLAUDE_CODE_SESSION_ID", "client_env": "AI_AGENT",
            "client_regexp": "^(claude-code)_(\\d+)-(\\d+)-(\\d+)_", "hook_session_from": "/session_id" },
  "integration": {
"skill": { "path": "~/.claude/skills/itse/SKILL.md", "name": "itse", "description": "Workspace facts from itse: tests, problems, symbols by name, debug snapshots" },
"mcp":   { "add": ["claude", "mcp", "add-json", "itse", "${json}", "--scope", "local"], "remove": ["claude", "mcp", "remove", "itse", "--scope", "local"],
           "cwd": "${workspaceFolder}", "value": {"type":"stdio","command":"itse","args":["mcp"]} },
"recipes": { "post-edit": { "file": "~/.claude/settings.json", "pointer": "/hooks/PostToolUse", "append":
    {"matcher":"Edit|Write|MultiEdit","hooks":[{"type":"command","command":"itse hook post-edit","timeout":10}]} } } },
  "hooks": { "post-edit": { "path_from": "/tool_input/file_path", "cwd_from": "/cwd", "verb": "problems", "flags": {"new": true},
         "wrap": {"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"${text}"}}, "quiet_when": "empty" } } }"#;

#[test]
fn a_full_kotlin_extension_parses() {
    let ext: Extension = serde_json::from_str(KOTLIN).unwrap();
    let lsp = &ext.packages["kotlin-lsp"];
    assert!(matches!(&lsp.pin_source, Some(PinSource::OpenvsxBundle { extension, .. }) if extension == "JetBrains/kotlin-server"));
    assert_eq!((lsp.consents.len(), lsp.exit_codes["7"].as_str()), (3, "server_expired"));
    assert_eq!(ext.servers[0].protocol, Protocol::Lsp);
    assert_eq!(ext.servers[1].via, Some(DapVia::LspCommand { server: "kotlin-lsp".into(), command: "start_debug_server".into(), arguments: vec![] }));
    assert!(matches!(&ext.problem_matchers["kotlinc"].pattern, Some(OneOrMany::One(p)) if p.message == Some(5)));
}

#[test]
fn a_gradle_action_and_a_workspace_layer_parse() {
    let action: Action = serde_json::from_str(GRADLE_TEST).unwrap();
    assert!(matches!(action.group, Some(Group::Full { is_default: true, .. })));
    assert!(matches!(&action.problem_matcher, Some(OneOrMany::Many(m)) if matches!(&m[0], MatcherRef::Name(n) if n == "$kotlinc")));
    assert!(action.extra.is_empty());
    let layer: Layer = serde_json::from_str(LAYER).unwrap();
    assert_eq!(layer.uses["node"]["install"], "path");
    assert_eq!(layer.servers["some-mcp"].max_list_bytes, Some(900));
}

#[test]
fn a_claude_code_extension_parses() {
    let ext: Extension = serde_json::from_str(CLAUDE_CODE).unwrap();
    assert_eq!(ext.identity.unwrap().hook_session_from.as_deref(), Some("/session_id"));
    assert_eq!(ext.hooks.unwrap()["post-edit"]["verb"], "problems");
}

#[test]
fn unknown_keys_are_refused_and_vs_code_ui_keys_kept_aside() {
    let err = serde_json::from_str::<Extension>(r#"{"itse_ext": 1, "id": "x", "usage": false}"#).unwrap_err();
    assert!(err.to_string().contains("unknown field `usage`"), "{err}");
    assert!(serde_json::from_str::<Layer>(r#"{"itse": 1, "usage": false}"#).is_err(), "a layer has no usage key");
    let action: Action = serde_json::from_str(r#"{"label": "b", "presentation": {"reveal": "always"}, "detail": "d"}"#).unwrap();
    assert_eq!(action.extra.keys().collect::<Vec<_>>(), ["detail", "presentation"]);
    assert!(action.extra.keys().all(|k| VS_CODE_UI_KEYS.contains(&k.as_str())));
}

#[test]
fn a_dap_adapter_is_reached_by_program_tcp_or_lsp_command() {
    let via = |s: &str| serde_json::from_str::<DapVia>(s).unwrap();
    assert_eq!(via(r#""program""#), DapVia::Program);
    assert_eq!(via(r#"{"tcp": {"argv": ["a"], "port": 5005}}"#), DapVia::Tcp { argv: vec!["a".into()], port: 5005 });
    assert!(serde_json::from_str::<DapVia>(r#"{"tcp": {"argv": [], "port": 1, "host": "10.0.0.1"}}"#).is_err());
}

#[test]
fn a_vs_code_group_may_be_a_string_and_a_pattern_a_list() {
    let action: Action =
        serde_json::from_str(r#"{"label": "t", "group": "test", "dependsOn": "b", "problemMatcher": {"pattern": [{"regexp": "a"}, {"regexp": "b", "message": 1}]}}"#).unwrap();
    assert!(matches!(action.group, Some(Group::Kind(k)) if k == "test"));
    assert_eq!(action.depends_on, Some(OneOrMany::One("b".into())));
    assert!(matches!(action.problem_matcher, Some(OneOrMany::One(MatcherRef::Inline(m))) if matches!(m.pattern, Some(OneOrMany::Many(ref p)) if p.len() == 2)));
}

// --- built-ins, loading, layers and templates ---

use std::collections::BTreeMap as Map2;
use std::ffi::OsString;
use std::path::Path;

use crate::sys::tests::TempDir;

/// A workspace with a `.git` dir under `tmp/work`, and the four dirs under `tmp/home`.
fn fixture(tmp: &TempDir) -> (Workspace, Dirs) {
    let vars: Map2<&str, OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    fs::create_dir_all(tmp.0.join("work/.git")).unwrap();
    (Workspace::find(Some(tmp.0.join("work")), &tmp.0).unwrap(), Dirs::from_env(|k| vars.get(k).cloned()).unwrap())
}

fn args(target: &str, rest: &[&str]) -> Args {
    Args { target: Some(target.into()), rest: rest.iter().map(|s| s.to_string()).collect(), ..Args::default() }
}

#[test]
fn every_ext_file_validates_and_is_a_built_in() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ext");
    let mut ids: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    ids.sort();
    assert_eq!(ids, BUILT_INS.map(|b| b.0.to_string()), "each ext/<id> is compiled in");
    for id in &ids {
        let path = dir.join(id).join("itse-ext.json");
        let ext = parse_ext(&fs::read_to_string(&path).unwrap(), &path.display().to_string()).unwrap();
        assert_eq!(&ext.id, id);
        assert!(ext.packages.values().all(|p| p.purl.starts_with("pkg:") && !p.purl.contains('@')), "{id}: launch data only, never pins");
    }
}

#[test]
fn rust_and_node_are_path_installs_and_the_jvm_ones_fetch_without_pins() {
    let tmp = TempDir::new("itse-manifest");
    let (ws, dirs) = fixture(&tmp);
    let m = Manifest::load(&ws, &dirs).unwrap();
    let install = |ext: &str, pkg: &str| m.exts[ext].0.packages[pkg].install.clone();
    assert_eq!((install("rust", "rust-analyzer"), install("node", "node")), ("path".into(), "path".into()));
    for (ext, pkg) in [("jdk", "jdk"), ("kotlin", "kotlin-lsp"), ("bun", "bun")] {
        assert_eq!(install(ext, pkg), "fetch");
        assert!(m.exts[ext].0.packages[pkg].pin_source.is_some(), "{pkg} names its pin source");
    }
    assert!(m.exts["gradle"].0.packages.is_empty() && m.exts["gradle"].0.requires == ["jdk"], "gradle runs the project's own gradlew");
    let cc = &m.exts["claude-code"].0;
    assert_eq!(cc.identity.as_ref().unwrap().session_env.as_deref(), Some("CLAUDE_CODE_SESSION_ID"));
    assert!(cc.integration.as_ref().unwrap()["mcp"]["add"].is_array() && cc.hooks.is_some());
}

#[test]
fn one_real_tasks_json_entry_parses() {
    // From VS Code's tasks documentation (code.visualstudio.com/docs/debugtest/tasks), unedited.
    let task = r#"{
      "label": "build",
      "type": "shell",
      "command": "msbuild",
      "args": [
        // Ask msbuild to generate full paths for file names.
        "/property:GenerateFullPaths=true",
        "/t:build",
        // Do not generate summary otherwise it leads to duplicate errors in Problems panel
        "/consoleloggerparameters:NoSummary"
      ],
      "group": "build",
      "presentation": {
        // Reveal the output only if unrecognized errors occur.
        "reveal": "silent"
      },
      // Use the standard MS compiler pattern to detect errors, warnings and infos
      "problemMatcher": "$msCompile"
    }"#;
    // tasks.json is JSONC; the comments go, the entry stays as it is.
    let json: String = task.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let ext = format!(r#"{{"itse_ext": 1, "id": "msbuild", "actions": [{json}]}}"#);
    let ext = parse_ext(&ext, "tasks.json").unwrap();
    let a = &ext.actions[0];
    assert_eq!((a.label.as_str(), a.kind.as_str(), a.args.len()), ("build", "shell", 3));
    assert!(matches!(&a.group, Some(Group::Kind(k)) if k == "build"));
    assert!(matches!(&a.problem_matcher, Some(OneOrMany::One(MatcherRef::Name(n))) if n == "$msCompile"));
    assert_eq!(a.extra.keys().collect::<Vec<_>>(), ["presentation"]);
}

#[test]
fn vs_code_ui_keys_are_ignored_and_unknown_keys_refused() {
    let tmp = TempDir::new("itse-manifest");
    let (ws, dirs) = fixture(&tmp);
    let ok = r#"{"itse_ext": 1, "id": "t", "actions": [{"label": "a", "command": "x", "presentation": {"reveal": "never"}, "runOptions": {}}]}"#;
    let ext = parse_ext(ok, "t.json").unwrap();
    assert_eq!(ext.actions[0].extra.keys().collect::<Vec<_>>(), ["presentation", "runOptions"]);
    let shell = r#"{"itse_ext": 1, "id": "t", "actions": [{"label": "a", "command": "x", "options": {"cwd": "w", "shell": {"executable": "bash"}}}]}"#;
    assert!(parse_ext(shell, "t.json").is_ok(), "options.shell is in the tasks.json schema, so it pastes");
    let err = parse_ext(r#"{"itse_ext": 1, "id": "t", "actions": [{"label": "a", "frobnicate": 1}]}"#, "/x/t.json").unwrap_err();
    assert_eq!(format!("{err:#}"), "extension /x/t.json: unknown field `frobnicate` in action 'a'");
    let err = parse_ext(r#"{"itse_ext": 1, "id": "t", "colour": 1}"#, "/x/t.json").unwrap_err();
    assert!(format!("{err:#}").starts_with("extension /x/t.json: unknown field `colour`"), "{err:#}");
    // A layer names its file too: the team's and the developer's.
    fs::create_dir_all(ws.root.join(".itse")).unwrap();
    fs::write(ws.root.join(".itse/itse.json"), r#"{"itse": 1, "usage": false}"#).unwrap();
    let err = format!("{:#}", Manifest::load(&ws, &dirs).err().unwrap());
    assert!(err.starts_with(&format!("team layer {}/.itse/itse.json: unknown field `usage`", ws.root.display())), "{err}");
    fs::remove_dir_all(ws.root.join(".itse")).unwrap();
    let dev = dirs.config.join("workspaces/work.json");
    fs::create_dir_all(dev.parent().unwrap()).unwrap();
    fs::write(&dev, format!(r#"{{"itse": 1, "repo": "{}", "uses": {{}}}}"#, ws.repo.display())).unwrap();
    let err = format!("{:#}", Manifest::load(&ws, &dirs).err().unwrap());
    assert!(err.starts_with(&format!("developer layer {}: unknown field `uses`", dev.display())), "{err}");
    fs::write(dirs.config.join("workspaces/other.json"), r#"{"itse": 1, "repo": "/elsewhere/.git", "junk": 1}"#).unwrap();
    fs::write(&dev, format!(r#"{{"itse": 1, "repo": "{}"}}"#, ws.repo.display())).unwrap();
    assert!(Manifest::load(&ws, &dirs).is_ok(), "another repo's broken file doesn't break this one");
}

#[test]
fn templates_fill_known_fields_and_refuse_others() {
    let mut fill = |k: &str| (k == "pkg:jdk").then(|| "/store/jdk".to_string());
    assert_eq!(render("${pkg:jdk}/bin/java -x", &mut fill).unwrap(), "/store/jdk/bin/java -x");
    assert_eq!(render("plain", &mut fill).unwrap(), "plain");
    assert_eq!(render("${pkg:kotlin-lsp}", &mut fill).unwrap_err().to_string(), "nothing fills ${pkg:kotlin-lsp} in '${pkg:kotlin-lsp}'");
    assert_eq!(render("${home}/x", &mut fill).unwrap_err().to_string(), "unknown template ${home} in '${home}/x'");
    assert!(render("${pkg:}", &mut fill).is_err() && render("${pkg:jdk", &mut fill).is_err());
    let bad = r#"{"itse_ext": 1, "id": "t", "servers": [{"id": "s", "command": ["${pkgs:x}"]}]}"#;
    assert!(format!("{:#}", parse_ext(bad, "t.json").unwrap_err()).contains("unknown template ${pkgs:x}"), "refused at load");
    let bad = r#"{"itse_ext": 1, "id": "t", "servers": [{"id": "s", "init_options": {"a": ["${home}"]}}]}"#;
    assert!(format!("{:#}", parse_ext(bad, "t.json").unwrap_err()).contains("unknown template ${home}"), "init_options are checked too");
}

#[test]
fn detection_suggests_and_use_requires_without_enabling_anything() {
    let tmp = TempDir::new("itse-manifest");
    let (ws, dirs) = fixture(&tmp);
    for f in ["Cargo.toml", "settings.gradle.kts"] {
        fs::write(ws.root.join(f), "").unwrap();
    }
    let m = Manifest::load(&ws, &dirs).unwrap();
    assert!(m.uses().is_empty(), "a detected extension is a suggestion");
    let found = m.detected(&ws.root, &[]);
    assert_eq!(found, [("gradle", "settings.gradle.kts"), ("kotlin", "settings.gradle.kts"), ("rust", "Cargo.toml")].map(|(a, b)| (a.to_string(), b.to_string())));
    use_ext(&ws, &dirs, &args("kotlin", &[]), true).unwrap();
    let uses: Vec<(String, Option<String>)> = Manifest::load(&ws, &dirs).unwrap().uses().into_iter().map(|u| (u.id, u.by)).collect();
    assert_eq!(uses, [("kotlin".to_string(), None), ("jdk".into(), Some("kotlin".into()))]);
}

#[test]
fn use_writes_one_private_file_found_by_its_repo_and_unuse_removes_it_when_empty() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("itse-manifest");
    let (ws, dirs) = fixture(&tmp);
    let o = use_ext(&ws, &dirs, &args("jdk", &["version=25.0.5+8", "install=fetch"]), true).unwrap();
    let path = dirs.config.join("workspaces/work.json");
    assert_eq!((o.head(), o.notes[0].as_str()), (format!("OK use jdk — {}", path.display()), r#"use.jdk = {"install":"fetch","version":"25.0.5+8"}"#));
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&path), mode(path.parent().unwrap())), (0o600, 0o700));
    let layer: Layer = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!((layer.repo.as_deref(), layer.uses["jdk"]["version"].as_str()), (Some(ws.repo.to_str().unwrap()), Some("25.0.5+8")));
    let o = use_ext(&ws, &dirs, &args("bun", &["version=1.10"]), true).unwrap();
    assert_eq!(o.notes[0], r#"use.bun = {"version":"1.10"}"#, "a version stays a string, its zero kept");
    use_ext(&ws, &dirs, &args("bun", &[]), false).unwrap();
    use_ext(&ws, &dirs, &args("node", &["install=path"]), true).unwrap();
    assert!(use_ext(&ws, &dirs, &args("node", &["colour=red"]), true).unwrap_err().to_string().starts_with("unknown setting 'colour=red' for node"));
    assert_eq!(use_ext(&ws, &dirs, &args("nope", &[]), true).unwrap_err().to_string(), "no extension 'nope'; 'itse ext list' lists them");
    use_ext(&ws, &dirs, &args("jdk", &[]), false).unwrap();
    assert!(path.exists(), "node is still in use");
    assert_eq!(use_ext(&ws, &dirs, &args("jdk", &[]), false).unwrap_err().to_string(), "jdk is not in the developer layer");
    use_ext(&ws, &dirs, &args("node", &[]), false).unwrap();
    assert!(!path.exists(), "an empty developer layer is removed");
    // Another repo of the same name gets its own file.
    fs::create_dir_all(dirs.config.join("workspaces")).unwrap();
    fs::write(&path, r#"{"itse": 1, "repo": "/elsewhere/work/.git", "use": {"rust": {}}}"#).unwrap();
    use_ext(&ws, &dirs, &args("node", &[]), true).unwrap();
    assert!(dirs.config.join("workspaces/work-2.json").exists());
}

#[test]
fn an_added_extension_loads_only_while_it_has_the_recorded_hash() {
    let tmp = TempDir::new("itse-manifest");
    let (ws, dirs) = fixture(&tmp);
    let file = tmp.0.join("hello.json");
    fs::write(&file, r#"{"itse_ext": 1, "id": "hello", "detect": ["hello.toml"]}"#).unwrap();
    let o = ext(&ws, &dirs, &tmp.0, &args("add", &["hello.json"]), "tty").unwrap();
    assert!(o.notes[0].starts_with("added hello (sha256 ") && o.notes[0].ends_with(") by tty"), "{:?}", o.notes);
    assert_eq!(lock::added(&dirs).unwrap()["hello"].actor, "tty");
    assert_eq!(Manifest::load(&ws, &dirs).unwrap().exts["hello"].1, "added");
    fs::write(dirs.config.join("extensions/hello.json"), r#"{"itse_ext": 1, "id": "hello", "detect": ["evil"]}"#).unwrap();
    let m = Manifest::load(&ws, &dirs).unwrap();
    assert!(!m.exts.contains_key("hello") && m.notes[0].ends_with("is not loaded: it differs from what 'itse ext add' recorded"));
    ext(&ws, &dirs, &tmp.0, &args("remove", &["hello"]), "non-tty").unwrap();
    assert!(!dirs.config.join("extensions/hello.json").exists() && lock::added(&dirs).unwrap().is_empty());
    assert!(ext(&ws, &dirs, &tmp.0, &args("remove", &["rust"]), "tty").unwrap_err().to_string().contains("built-ins can't be removed"));
    fs::create_dir_all(dirs.config.join("workspaces")).unwrap();
    fs::write(dirs.config.join("workspaces/w.json"), "{}").unwrap();
    let bad = ext(&ws, &dirs, &tmp.0, &args("remove", &["../workspaces/w"]), "tty").unwrap_err().to_string();
    assert_eq!((bad.as_str(), dirs.config.join("workspaces/w.json").exists()), ("extension id '../workspaces/w' is not a plain name", true));
    let shown = ext(&ws, &dirs, &tmp.0, &args("show", &["kotlin"]), "tty").unwrap().notes.join("\n");
    assert!(shown.contains("packages: kotlin-lsp (fetch, runs project code)") && shown.contains("requires: jdk"), "{shown}");
}
