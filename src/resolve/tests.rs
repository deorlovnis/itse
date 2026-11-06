use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::manifest::{ext, use_ext};
use crate::results::Verdict;
use crate::sys::tests::TempDir;

/// A git workspace under `tmp/work`, the four dirs under `tmp/home`, and an empty `tmp/bin` as PATH.
struct Fx {
    tmp: TempDir,
    ws: Workspace,
    dirs: Dirs,
    env: BTreeMap<String, String>,
}

impl Fx {
    fn new() -> Self {
        let tmp = TempDir::new("itse-resolve");
        let vars: BTreeMap<&str, OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
        fs::create_dir_all(tmp.0.join("work/.git")).unwrap();
        fs::create_dir_all(tmp.0.join("bin")).unwrap();
        let (ws, dirs) = (Workspace::find(Some(tmp.0.join("work")), &tmp.0).unwrap(), Dirs::from_env(|k| vars.get(k).cloned()).unwrap());
        let env = [("PATH".to_string(), tmp.0.join("bin").display().to_string())].into();
        Self { tmp, ws, dirs, env }
    }

    fn resolve(&self) -> Resolved {
        self.resolve_at(SystemTime::now())
    }

    fn resolve_at(&self, now: SystemTime) -> Resolved {
        resolve(&self.ws, &self.dirs, &self.env, now).unwrap()
    }

    fn row(&self, item: &str) -> State {
        let r = self.resolve();
        r.states.iter().find(|s| s.item == item).cloned().unwrap_or_else(|| panic!("no {item} in {:?}", r.states))
    }

    fn use_(&self, ext: &str, settings: &[&str]) {
        let args = Args { target: Some(ext.into()), rest: settings.iter().map(|s| s.to_string()).collect(), ..Args::default() };
        use_ext(&self.ws, &self.dirs, &args, true).unwrap();
    }

    fn trust(&self) {
        lock::trust(&self.dirs, &self.ws, &self.resolve().trust_hash, false, "tty").unwrap();
    }

    /// An executable `bin/<name>`.
    fn tool(&self, name: &str) -> PathBuf {
        let path = self.tmp.0.join("bin").join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Pin `pkg` in the machine lock, and put it in the store when `stored`; returns its store dir.
    fn pin(&self, pkg: &str, sha: &str, expires: Option<&str>, stored: bool) -> PathBuf {
        let path = lock::repo_dir(&self.dirs, &self.ws).join("itse.lock.json");
        let mut lock: Value = lock::read(&path).unwrap();
        lock["itse_lock"] = 1.into();
        lock["packages"][pkg] = json!({"purl": format!("pkg:generic/x/{pkg}@1.0"), "install": "fetch", "expires_estimate": expires, "extension": "x@0.1.0",
            "targets": {target(): {"url": format!("https://example.test/{pkg}.tar.gz"), "sha256": sha, "size": 412_000_000, "provenance": "upstream:github-digest"}}});
        lock::write(&path, &lock).unwrap();
        let store = self.dirs.data.join("store").join(format!("sha256-{sha}"));
        if stored {
            fs::create_dir_all(&store).unwrap();
        }
        store
    }
}

fn next(s: &State) -> Vec<&str> {
    s.next.iter().flatten().map(String::as_str).collect()
}

#[test]
fn a_fresh_repo_shows_suggestions_with_itse_use_as_next() {
    let fx = Fx::new();
    fs::write(fx.ws.root.join("Cargo.toml"), "").unwrap();
    fs::write(fx.ws.root.join("package.json"), "{}").unwrap();
    let r = fx.resolve();
    let rows: Vec<(&str, &str, Vec<&str>)> = r.states.iter().map(|s| (s.item.as_str(), s.state.as_str(), next(s))).collect();
    assert_eq!(rows, [("node", "suggested", vec!["use", "node"]), ("rust", "suggested", vec!["use", "rust"])]);
    assert_eq!((r.states[1].actor.as_deref(), r.states[1].reason.as_deref()), (Some("agent"), Some("Cargo.toml found")));
    assert!(r.launches.is_empty() && r.notes.contains(&"layer: none; 'itse use <ext>' declares one".into()));
    let mut o = Outcome { states: r.states, ..Outcome::new("status", Verdict::Ok, "w", "") };
    let mcp = o.render("mcp", 1, crate::results::VERB, false).unwrap();
    assert!(mcp.contains(r#"suggested      rust                 Cargo.toml found; next (agent): test_build_run {verb: "use", args: ["rust"]}"#), "{mcp}");
}

#[test]
fn status_reports_a_missing_server_with_a_remedy() {
    let fx = Fx::new();
    fx.use_("kotlin", &[]);
    let s = fx.row("kotlin-lsp");
    assert_eq!((s.state.as_str(), next(&s), s.actor.as_deref()), ("missing", vec!["lock", "--refresh", "kotlin-lsp"], Some("agent")));
    assert_eq!(s.hosts, ["open-vsx.org", "download.jetbrains.com"]);
    fx.pin("kotlin-lsp", "877aee61dc96e", None, false);
    let s = fx.row("kotlin-lsp 1.0");
    assert_eq!((s.state.as_str(), s.reason.as_deref(), next(&s)), ("missing", Some("not in store; 412 MB"), vec!["install", "kotlin-lsp"]));
    assert_eq!(fx.row("jdk").state, "missing", "kotlin requires jdk");
    assert!(fx.resolve().launches.iter().all(|l| l.id != "kotlin-lsp"), "a missing server is reported, never started");
}

#[test]
fn untrusted_workspace_runs_no_project_code() {
    let fx = Fx::new();
    let ra = fx.tool("rust-analyzer");
    fx.tool("cargo");
    fx.use_("rust", &[]);
    let r = fx.resolve();
    let ws = r.states.iter().find(|s| s.item == "workspace").unwrap();
    assert_eq!(
        (ws.state.as_str(), ws.reason.as_deref(), next(ws), ws.actor.as_deref()),
        ("untrusted", Some("project code runs in rust-analyzer, cargo:build, cargo:test"), vec!["trust"], Some("human"))
    );
    assert_eq!(fx.row("rust-analyzer").state, "unpinned");
    assert!(r.launches.is_empty(), "rust-analyzer runs build scripts, so it waits for trust");
    fx.trust();
    let r = fx.resolve();
    assert_eq!(r.launches.len(), 1);
    assert_eq!((r.launches[0].argv.clone(), &r.launches[0].cwd), (vec![ra.display().to_string()], &fx.ws.root));
    // Its cargo check builds under the cache, per workspace, never in the repo's target/.
    let target = fx.dirs.cache.join("rust-analyzer").join(&fx.ws.ws).display().to_string();
    assert_eq!((&r.launches[0].settings["rust-analyzer"]["cargo"]["targetDir"], &r.launches[0].init_options["cargo"]["targetDir"]), (&json!(target), &json!(target)));
    // A type whose name fuzzy-matches a function's would hide it under rust-analyzer's default (types only).
    let search = json!({"kind": "all_symbols", "limit": 1000});
    assert_eq!((&r.launches[0].settings["rust-analyzer"]["workspace"]["symbol"]["search"], &r.launches[0].init_options["workspace"]["symbol"]["search"]), (&search, &search));
    assert_eq!(r.launches[0].init_options, r.launches[0].settings["rust-analyzer"], "its init options are its settings section, declared once");
    assert_eq!(r.launches[0].outputs, ["Cargo.lock"], "its cargo metadata makes a missing lock file");
    let found: BTreeMap<String, lock::Found> = lock::read(&fx.dirs.state.join("found.json")).unwrap();
    assert_eq!((found["rust-analyzer"].path.as_str(), found["rust-analyzer"].sha256.as_str()), (ra.to_str().unwrap(), sys::sha256_hex(b"#!/bin/sh\n").as_str()));
}

#[test]
fn a_path_tool_not_on_path_is_missing_with_who_can_help() {
    let fx = Fx::new();
    fx.use_("rust", &[]);
    fx.use_("node", &[]);
    let ra = fx.row("rust-analyzer");
    assert_eq!((ra.state.as_str(), ra.reason.as_deref(), next(&ra)), ("missing", Some("rust-analyzer not on PATH"), vec!["use", "rust", "install=fetch"]));
    let node = fx.row("node");
    assert_eq!((node.actor.as_deref(), node.next.is_none()), (Some("halti-owner"), true), "node has no pin source; the image provides it");
}

#[test]
fn refresh_does_not_revoke_trust() {
    let fx = Fx::new();
    fx.tool("rust-analyzer");
    fx.use_("rust", &[]);
    fx.use_("jdk", &[]);
    fx.trust();
    let before = fx.resolve().trust_hash;
    // What `lock --refresh` and `install` change: pins, expiry estimates and the store.
    fx.pin("jdk", "aa11", Some("2027-01-01"), true);
    fx.pin("rust-analyzer", "bb22", Some("2027-01-01"), true);
    fx.use_("jdk", &["version=25.0.5+8"]);
    let r = fx.resolve();
    assert_eq!((r.trust_hash, r.trusted), (before, true));
    assert_eq!(fx.row("workspace").state, "ready");
    // A new project-code entry asks again.
    fx.use_("kotlin", &[]);
    assert_eq!(fx.row("workspace").state, "untrusted");
}

#[test]
fn a_team_layer_change_asks_for_trust_again() {
    let fx = Fx::new();
    fs::create_dir_all(fx.ws.root.join(".itse")).unwrap();
    let team = fx.ws.root.join(".itse/itse.json");
    fs::write(&team, r#"{"itse": 1, "use": {"node": {}}}"#).unwrap();
    let w = fx.row("workspace");
    assert_eq!((w.state.as_str(), w.reason.as_deref()), ("untrusted", Some("project code runs in the team layer .itse/")));
    assert_eq!(fx.row("node").state, "missing", "the team layer is read before trust");
    fx.trust();
    assert_eq!(fx.row("workspace").state, "ready");
    fs::write(&team, r#"{"itse": 1, "use": {"node": {}, "rust": {}}}"#).unwrap();
    assert_eq!(fx.row("workspace").state, "untrusted");
    fx.trust();
    fs::write(&team, r#"{"itse": 1, "use": {"node": {}}}"#).unwrap();
    assert_eq!(fx.row("workspace").state, "ready", "switching back to an accepted branch doesn't ask");
    let lock = fx.ws.root.join(".itse/itse.lock.json");
    fs::write(&lock, r#"{"itse_lock": 1}"#).unwrap();
    assert_eq!(fx.row("workspace").state, "untrusted", "the team lock is a team-layer file");
}

#[test]
fn expiring_shows_seven_days_ahead() {
    let fx = Fx::new();
    fx.use_("bun", &[]);
    let now = SystemTime::now();
    let day = |d: i64| sys::utc(if d < 0 { now - Duration::from_secs(-d as u64 * 86_400) } else { now + Duration::from_secs(d as u64 * 86_400) })[..10].to_string();
    let state = |d: i64| {
        fx.pin("bun", "cc33", Some(&day(d)), true);
        let r = fx.resolve_at(now);
        let s = r.states.iter().find(|s| s.item == "bun 1.0").unwrap().clone();
        (s.state.clone(), next(&s).join(" "))
    };
    assert_eq!(state(8), ("ready".into(), String::new()));
    assert_eq!(state(7), ("expiring".into(), "lock --refresh bun".into()));
    assert_eq!(state(1), ("expiring".into(), "lock --refresh bun".into()));
    assert_eq!(state(0), ("expiring".into(), "lock --refresh bun".into()), "expires today");
    assert_eq!(state(-1), ("expired".into(), "lock --refresh bun".into()));
}

#[test]
fn consent_is_asked_for_recorded_and_asked_again_for_a_new_eula() {
    let fx = Fx::new();
    fx.use_("kotlin", &[]);
    let store = fx.pin("kotlin-lsp", "dd44", None, true);
    fx.pin("jdk", "ee55", None, true);
    fs::write(store.join("EULA.txt"), "terms v1").unwrap();
    let s = fx.row("kotlin-lsp 1.0");
    assert_eq!(
        (s.state.as_str(), s.reason.as_deref(), next(&s), s.actor.as_deref()),
        ("needs_consent", Some("eula, region, data_sharing"), vec!["consent", "kotlin-lsp"], Some("human"))
    );
    let consent = |flags: &[(&str, &str)], tty: bool| {
        let flags = flags.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect();
        fx.resolve().consent(&fx.dirs, &Args { target: Some("kotlin-lsp".into()), flags, ..Args::default() }, tty).unwrap()
    };
    let o = consent(&[], false);
    assert_eq!(o.head(), "NEEDS_HUMAN consent kotlin-lsp — a human runs this; next (human): itse consent kotlin-lsp in a terminal");
    assert!(!fx.dirs.state.join("consents.json").exists(), "nothing recorded without a terminal");
    let hash = sys::sha256_hex(b"terms v1")[..16].to_string();
    let o = consent(&[("region", "eu")], true);
    assert_eq!(o.verdict, Verdict::NeedsConsent);
    assert_eq!(o.notes, [format!("--eula-hash {hash} accepts {}", store.join("EULA.txt").display()), "--data-sharing none|anonymous|full".into()]);
    assert_eq!(consent(&[("eula-hash", &hash), ("region", "eu"), ("data-sharing", "lots")], true).notes, ["--data-sharing none|anonymous|full"]);
    let o = consent(&[("eula-hash", &hash), ("region", "eu"), ("data-sharing", "none")], true);
    assert_eq!(o.head(), "OK consent kotlin-lsp — recorded eula, region, data_sharing by tty");
    fx.trust();
    assert_eq!(fx.row("kotlin-lsp 1.0").state, "ready");
    let r = fx.resolve();
    let lsp = r.launches.iter().find(|l| l.id == "kotlin-lsp").expect("ready and trusted, so it may start");
    assert_eq!(lsp.argv.last().unwrap(), &format!("--eula={hash}"));
    assert_eq!(lsp.env, [("JAVA_HOME".to_string(), fx.dirs.data.join("store/sha256-ee55").display().to_string())]);
    assert_eq!(lsp.exit_codes.get(&7).map(String::as_str), Some("server_expired"));
    fs::write(store.join("EULA.txt"), "terms v2").unwrap();
    assert_eq!(fx.row("kotlin-lsp 1.0").reason.as_deref(), Some("eula"), "a new EULA asks again");
}

#[test]
fn system_checks_are_typed_and_a_team_command_waits_for_trust() {
    let fx = Fx::new();
    let ran = fx.tmp.0.join("ran");
    fs::create_dir_all(fx.ws.root.join(".itse")).unwrap();
    let team = json!({"itse": 1, "system": {"probe": {"cmd": ["touch", ran]}, "shared": {"cmd": ["false"]}}});
    fs::write(fx.ws.root.join(".itse/itse.json"), team.to_string()).unwrap();
    let dev =
        json!({"itse": 1, "repo": fx.ws.repo, "system": {"ok": {"cmd": ["true"]}, "no": {"cmd": ["false"]}, "heif": {"lib": "libnope-itse.so.9"}, "shared": {"cmd": ["true"]}}});
    lock::write(&fx.dirs.config.join("workspaces/work.json"), &dev).unwrap();
    let rows: BTreeMap<String, (String, String, Option<String>)> =
        fx.resolve().states.into_iter().filter(|s| s.item.ends_with("(system)")).map(|s| (s.item, (s.state, s.reason.unwrap_or_default(), s.actor))).collect();
    assert_eq!(rows["ok (system)"].0, "ready");
    assert_eq!(rows["no (system)"], ("blocked".into(), "false exit 1; needs_image_layer no".into(), Some("halti-owner".into())));
    assert_eq!(rows["heif (system)"].1, "libnope-itse.so.9 not found; needs_image_layer heif");
    assert_eq!(rows["shared (system)"].0, "ready", "the developer's entry replaces the team's by name");
    assert_eq!(rows["probe (system)"].0, "untrusted");
    assert!(!ran.exists(), "a team-layer command doesn't run before trust");
    fx.trust();
    let quick = resolve_for(&fx.ws, &fx.dirs, &fx.env, SystemTime::now(), false).unwrap();
    assert!(!ran.exists() && !quick.states.iter().any(|s| s.item.ends_with("(system)")), "a shim's resolve runs no check");
    assert_eq!(fx.row("probe (system)").state, "ready");
    assert!(ran.exists());
}

#[test]
fn a_check_whose_child_is_reaped_elsewhere_says_so() {
    // SIGCHLD ignored reaps every child of the process, so the check runs in a test process of its own.
    const NAME: &str = "resolve::tests::a_check_whose_child_is_reaped_elsewhere_says_so";
    if std::env::var_os("ITSE_TEST_SIGCHLD_IGNORED").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", NAME]).env("ITSE_TEST_SIGCHLD_IGNORED", "1").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        return assert!(out.status.success() && text.contains("1 passed"), "{text}{}", String::from_utf8_lossy(&out.stderr));
    }
    unsafe extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    // SAFETY: SIG_IGN (1) for SIGCHLD (17) installs no handler.
    unsafe { signal(17, 1) };
    for cmd in [vec!["true"], vec!["sleep", "0.3"]] {
        let check = SystemCheck { lib: None, cmd: cmd.iter().map(|s| s.to_string()).collect() };
        let err = system_check(&check).unwrap_err();
        assert!(err.starts_with(&format!("{}: ", cmd.join(" "))) && !err.contains("exit") && !err.contains("took over"), "{err}");
    }
}

#[test]
fn a_package_from_path_is_rooted_where_its_bin_path_starts() {
    let fx = Fx::new();
    fx.use_("jdk", &["install=path"]);
    let jdk = fx.tmp.0.join("jdk-25");
    fs::create_dir_all(jdk.join("bin")).unwrap();
    fs::write(jdk.join("bin/java"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(jdk.join("bin/java"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(jdk.join("bin/java"), fx.tmp.0.join("bin/java")).unwrap();
    assert_eq!(fx.resolve().roots["jdk"], fs::canonicalize(&jdk).unwrap(), "through the PATH link to the JDK above bin/java");
    let other = fx.tmp.0.join("tools");
    fs::create_dir_all(&other).unwrap();
    fs::rename(jdk.join("bin/java"), other.join("java")).unwrap();
    let env = [("PATH".to_string(), other.display().to_string())].into();
    assert_eq!(resolve(&fx.ws, &fx.dirs, &env, SystemTime::now()).unwrap().roots["jdk"], other, "a java in no bin/ is rooted in its folder");
}

#[test]
fn project_pins_are_read_never_written() {
    let fx = Fx::new();
    fx.use_("gradle", &[]);
    let props = fx.ws.root.join("gradle/wrapper/gradle-wrapper.properties");
    fs::create_dir_all(props.parent().unwrap()).unwrap();
    fs::write(&props, "distributionUrl=https\\://services.gradle.org/distributions/gradle-9.8.0-bin.zip\n").unwrap();
    fs::write(fx.ws.root.join(".java-version"), "\n25\n").unwrap();
    let before = fs::metadata(&props).unwrap().modified().unwrap();
    let s = fx.row("gradle-wrapper (project)");
    let want = "gradle/wrapper/gradle-wrapper.properties: distributionUrl https://services.gradle.org/distributions/gradle-9.8.0-bin.zip; distributionSha256Sum absent";
    assert_eq!((s.state.as_str(), s.reason.as_deref()), ("ready", Some(want)));
    assert_eq!(fx.row("java-version (project)").reason.as_deref(), Some(".java-version: 25"), "jdk comes in through requires");
    assert_eq!(fs::metadata(&props).unwrap().modified().unwrap(), before);
}

#[test]
fn an_unknown_extension_in_a_layer_is_a_row_not_a_crash() {
    let fx = Fx::new();
    lock::write(&fx.dirs.config.join("workspaces/work.json"), &json!({"itse": 1, "repo": fx.ws.repo, "use": {"cobol": {}}})).unwrap();
    let s = fx.row("cobol (extension)");
    assert_eq!((s.state.as_str(), next(&s)), ("missing", vec!["ext", "list"]));
    let tmp = TempDir::new("itse-resolve-ext");
    fs::write(tmp.0.join("cobol.json"), r#"{"itse_ext": 1, "id": "cobol"}"#).unwrap();
    ext(&fx.ws, &fx.dirs, &tmp.0, &Args { target: Some("add".into()), rest: vec!["cobol.json".into()], ..Args::default() }, "tty").unwrap();
    assert!(fx.resolve().states.iter().all(|s| s.item != "cobol (extension)"));
}

#[test]
fn a_team_lock_alone_asks_for_trust_and_its_pins_wait_for_it() {
    let fx = Fx::new();
    fx.use_("bun", &[]);
    let team = fx.ws.root.join(".itse/itse.lock.json");
    fs::create_dir_all(team.parent().unwrap()).unwrap();
    let pin = json!({"itse_lock": 1, "packages": {"bun": {"purl": "pkg:generic/x/bun@9.9", "install": "fetch", "targets": {target(): {"url": "https://example.test/bun.zip", "sha256": "abcd"}}}}});
    fs::write(&team, pin.to_string()).unwrap();
    fs::create_dir_all(fx.dirs.data.join("store/sha256-abcd")).unwrap();
    let r = fx.resolve();
    let ws = r.states.iter().find(|s| s.item == "workspace").unwrap();
    assert_eq!((ws.state.as_str(), ws.reason.as_deref()), ("untrusted", Some("project code runs in the team layer .itse/")));
    let bun = fx.row("bun");
    assert_eq!((bun.state.as_str(), bun.reason.as_deref(), next(&bun)), ("untrusted", Some("pinned only by the team lock .itse/itse.lock.json"), vec!["trust"]));
    assert!(!r.roots.contains_key("bun"), "the repo's pin chooses nothing before trust");
    fx.trust();
    assert_eq!(fx.row("bun 9.9").state, "ready");
}

#[test]
fn install_fetch_moves_only_the_packages_that_have_a_pin_source() {
    let fx = Fx::new();
    fx.tool("cargo");
    fx.use_("rust", &["install=fetch"]);
    assert_eq!(fx.row("cargo").state, "unpinned", "cargo has no pin source, so it stays a path install");
    assert_eq!(next(&fx.row("rust-analyzer")), ["lock", "--refresh", "rust-analyzer"]);
}

#[test]
fn team_server_settings_wait_for_trust_and_init_options_are_rendered() {
    let fx = Fx::new();
    let tmp = TempDir::new("itse-resolve-ext");
    let hello = json!({"itse_ext": 1, "id": "hello", "packages": {"hello-ls": {"install": "path"}},
        "servers": [{"id": "hello-ls", "command": ["${pkg:hello-ls}/hello-ls"], "init_options": {"root": "${workspaceFolder}", "n": [1, "${cache}"]}}]});
    fs::write(tmp.0.join("hello.json"), hello.to_string()).unwrap();
    ext(&fx.ws, &fx.dirs, &tmp.0, &Args { target: Some("add".into()), rest: vec!["hello.json".into()], ..Args::default() }, "tty").unwrap();
    fx.tool("hello-ls");
    fx.use_("hello", &[]);
    fs::create_dir_all(fx.ws.root.join(".itse")).unwrap();
    fs::write(fx.ws.root.join(".itse/itse.json"), r#"{"itse": 1, "servers": {"hello-ls": {"expose": true, "settings": {"x": 1}}}}"#).unwrap();
    let l = &fx.resolve().launches[0];
    assert_eq!((l.expose, &l.settings), (false, &Value::Null), "an untrusted team layer sets nothing");
    let (root, cache) = (fx.ws.root.display().to_string(), fx.dirs.cache.display().to_string());
    assert_eq!(l.init_options, json!({"root": root, "n": [1, cache]}));
    fx.trust();
    let l = &fx.resolve().launches[0];
    assert_eq!((l.expose, &l.settings), (true, &json!({"x": 1})));
}
