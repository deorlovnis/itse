//! The store end to end: `install` through a fake curl (itse-fixture behind
//! ITSE_TEST_DELEGATE_DIR) with the real tar and gzip, shims linked to this CLI, `env` and `exec`,
//! `lock --refresh` against recorded API JSON, `status --hosts`, the door's refusals, and the
//! daemon's start-up clean of leftover `.tmp` folders. `ITSE_E2E_FETCH=1` runs the real thing
//! through the door.

use std::os::unix::fs::PermissionsExt;

use super::*;

/// This machine's lock target, as the resolver names it.
fn target() -> &'static str {
    if std::env::consts::ARCH == "aarch64" { "linux_arm64" } else { "linux_x64" }
}

/// `itse ARGS` in `cwd` with the fake delegates, serving `files/`: (exit code, stdout, stderr).
fn itse(env: &Env, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let delegates = env.dir.join("delegates");
    if !delegates.exists() {
        fs::create_dir_all(&delegates).unwrap();
        fs::write(delegates.join("curl"), format!("#!/bin/sh\nexec {FIXTURE} tool curl \"$@\"\n")).unwrap();
        fs::set_permissions(delegates.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = env.command(cwd, args);
    cmd.env("ITSE_TEST_DELEGATE_DIR", &delegates).env("ITSE_FIXTURE_FILES", env.dir.join("files")).env("ITSE_FIXTURE_LOG", env.dir.join("delegates.log"));
    let out = cmd.envs(extra.iter().copied()).output().unwrap();
    (out.status.code(), stdout(&out), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Serve `bytes` at `https://<rest>` through the fake curl.
fn serve(env: &Env, rest: &str, bytes: &[u8]) {
    let path = env.dir.join("files").join(rest);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn gz(text: &str) -> Vec<u8> {
    let mut child = Command::new("gzip").arg("-c").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    child.wait_with_output().unwrap().stdout
}

fn sha256(bytes: &[u8]) -> String {
    let mut child = Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()[..64].to_string()
}

fn rid(env: &Env, cwd: &Path) -> String {
    let text = itse(env, cwd, &["status"], &[]).1;
    text.split("(rid ").nth(1).and_then(|r| r.get(..12)).unwrap_or_else(|| panic!("no rid in {text}")).to_string()
}

/// Pin rust-analyzer in the repo of `cwd` at `url` with `bytes`' sha256.
fn pin(env: &Env, cwd: &Path, url: &str, sha: &str, size: usize) {
    let lock = json!({"itse_lock": 1, "packages": {"rust-analyzer": {"purl": "pkg:github/rust-lang/rust-analyzer@2026-11-02", "install": "fetch", "extension": "rust@0.1.0",
        "targets": {target(): {"url": url, "sha256": sha, "size": size, "provenance": "upstream:github-digest"}}}}});
    let path = env.dir.join(format!("state/itse/repo/{}/itse.lock.json", rid(env, cwd)));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lock.to_string()).unwrap();
}

/// A work repo using rust (fetched) and trusting it, with the fake rust-analyzer pinned and
/// installed; its store folder.
fn installed(env: &Env) -> PathBuf {
    let work = env.work();
    fs::create_dir_all(work.join(".git")).unwrap();
    assert_eq!(itse(env, &work, &["use", "rust", "install=fetch"], &[]).0, Some(0));
    let bytes = gz("#!/bin/sh\necho \"store rust-analyzer $*\"\n");
    serve(env, "files.test/ra.gz", &bytes);
    pin(env, &work, "https://files.test/ra.gz", &sha256(&bytes), bytes.len());
    assert_eq!(itse(env, &work, &["trust"], &[]).0, Some(0));
    let (code, text, err) = itse(env, &work, &["install"], &[]);
    assert_eq!(code, Some(0), "{text}{err}");
    let store = env.dir.join(format!("data/itse/store/sha256-{}", sha256(&bytes)));
    assert!(text.starts_with(&format!("OK install {} — 1 of 1 ready", work.display())), "{text}");
    assert!(text.contains(&format!("\nready          rust-analyzer 2026-11-02 store {}\n", store.display())), "{text}");
    store
}

/// Run the shim `name` in `cwd` with `path` as PATH.
fn shim(env: &Env, name: &str, cwd: &Path, path: &str) -> Output {
    let link = env.dir.join("data/itse/bin").join(name);
    env.command_of(link.to_str().unwrap(), cwd, &["--version"]).env("PATH", path).output().unwrap()
}

#[test]
fn install_verifies_stores_and_links_shims_to_the_cli() {
    let env = Env::new();
    let store = installed(&env);
    assert_eq!(fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o555);
    assert_eq!(fs::read_link(env.dir.join("data/itse/bin/rust-analyzer")).unwrap(), fs::canonicalize(ITSE).unwrap(), "the shim is the CLI's own path");
    let log = fs::read_to_string(env.dir.join("delegates.log")).unwrap();
    let curl: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    let part = env.dir.join(format!("data/itse/dl/{}.part", store.file_name().unwrap().to_str().unwrap().trim_start_matches("sha256-")));
    let w = "%{http_connect} %{http_code} %{url_effective}";
    let want = json!(["--fail-with-body", "-sSL", "--proto", "=https", "--proto-redir", "=https", "-w", w, "-C", "-", "-o", part, "https://files.test/ra.gz"]);
    assert_eq!(curl["argv"], want);
    assert!(!part.exists());
    let (code, text, _) = itse(&env, &env.work(), &["install", "rust-analyzer"], &[("HTTPS_PROXY", "http://127.0.0.1:9")]);
    assert_eq!((code, text.lines().count()), (Some(0), 2), "the store's copy needs no fetch: {text}");
    assert_eq!(fs::read_to_string(env.dir.join("delegates.log")).unwrap().lines().count(), 1);
}

#[test]
fn a_shim_execs_the_store_binary_or_the_next_on_path() {
    let env = Env::new();
    installed(&env);
    let shims = env.dir.join("data/itse/bin");
    let path = format!("{}:/usr/bin:/bin", shims.display());
    let out = shim(&env, "rust-analyzer", &env.work().join(".git"), &path);
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), "store rust-analyzer --version\n"), "{}", String::from_utf8_lossy(&out.stderr));
    // Another repo that doesn't use rust: the next rust-analyzer on PATH, past the shims.
    let (other, bin) = (env.dir.join("other"), env.dir.join("pathbin"));
    fs::create_dir_all(other.join(".git")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let out = shim(&env, "rust-analyzer", &other, &path);
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(String::from_utf8_lossy(&out.stderr), format!("itse: rust-analyzer is not managed in {} and not on PATH\n", other.display()));
    fs::write(bin.join("rust-analyzer"), "#!/bin/sh\necho \"path rust-analyzer $*\"\n").unwrap();
    fs::set_permissions(bin.join("rust-analyzer"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = shim(&env, "rust-analyzer", &other, &format!("{}:{}:/usr/bin:/bin", shims.display(), bin.display()));
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), "path rust-analyzer --version\n"));
    // Declared there but not pinned: one line with the remedy and the hosts, exit 127.
    assert_eq!(itse(&env, &other, &["use", "rust", "install=fetch"], &[]).0, Some(0));
    let out = shim(&env, "rust-analyzer", &other, &path);
    assert_eq!(out.status.code(), Some(127));
    let want = format!(
        "itse: rust-analyzer missing (no pin for {}); run 'itse lock --refresh rust-analyzer' (needs hosts api.github.com, github.com, release-assets.githubusercontent.com)\n",
        target()
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), want);
    // Untrusted: rust-analyzer runs project code, so its shim refuses.
    assert_eq!(itse(&env, &env.work(), &["trust", "--revoke"], &[]).0, Some(0));
    let out = shim(&env, "rust-analyzer", &env.work(), &path);
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "itse: workspace untrusted (project code runs in rust-analyzer, cargo:build, cargo:test); run 'itse trust'\n");
}

#[test]
fn a_shim_of_a_package_from_path_runs_what_path_finds() {
    let env = Env::new();
    let work = env.work();
    fs::create_dir_all(work.join(".git")).unwrap();
    // A java straight in a PATH folder, though the jdk's bin is `bin/java`.
    let (bin, shims) = (env.dir.join("pathbin"), env.dir.join("data/itse/bin"));
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("java"), "#!/bin/sh\necho \"path java $*\"\n").unwrap();
    fs::set_permissions(bin.join("java"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(fs::canonicalize(ITSE).unwrap(), shims.join("java")).unwrap();
    let path = format!("{}:{}:/usr/bin:/bin", shims.display(), bin.display());
    let out = env.command(&work, &["use", "jdk", "install=path"]).env("PATH", &path).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let out = shim(&env, "java", &work, &path);
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), "path java --version\n"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn env_and_exec_put_the_store_bins_first() {
    let env = Env::new();
    let store = installed(&env);
    let lang = [("LANG", ""), ("LC_ALL", "")];
    let mut cmd = env.command(&env.work(), &["env", "--sh"]);
    let out = cmd.env("PATH", "/usr/bin:/bin").env_remove("LANG").env_remove("LC_ALL").output().unwrap();
    assert_eq!(stdout(&out), format!("export LANG='C.UTF-8'\nexport PATH='{}:/usr/bin:/bin'\n", store.display()));
    let (code, text, err) = itse(&env, &env.work(), &["exec", "--", "sh", "-c", "echo $PATH; rust-analyzer x"], &lang);
    assert_eq!(code, Some(0), "{err}");
    assert!(text.starts_with(&format!("{}:", store.display())) && text.ends_with("store rust-analyzer x\n"), "{text}");
    let (code, _, err) = itse(&env, &env.work(), &["exec"], &[]);
    assert_eq!((code, err.as_str()), (Some(2), "itse: missing argument <cmd>\n"));
}

#[test]
fn install_reports_a_door_refusal_with_the_host_and_its_actor() {
    let env = Env::new();
    let work = env.work();
    fs::create_dir_all(work.join(".git")).unwrap();
    assert_eq!(itse(&env, &work, &["use", "rust", "install=fetch"], &[]).0, Some(0));
    pin(&env, &work, "https://blocked.test/ra.gz", &"a".repeat(64), 10);
    let (code, text, _) = itse(&env, &work, &["install"], &[]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with(&format!("BLOCKED install {} — 0 of 1 ready", work.display())), "{text}");
    assert!(text.contains("\nblocked        rust-analyzer 2026-11-02 blocked_by_door host blocked.test; actor halti-owner; hosts api.github.com"), "{text}");
    assert!(!env.dir.join("data/itse/bin/rust-analyzer").exists());
    // In the tunnel, the door's own answer is a refusal; the site's own 403 is the agent's.
    pin(&env, &work, "https://nowrite.test/ra.gz", &"a".repeat(64), 10);
    let (code, text, _) = itse(&env, &work, &["install"], &[]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("door_refused_write host nowrite.test; actor halti-owner"), "{text}");
    pin(&env, &work, "https://ratelimit.test/ra.gz", &"a".repeat(64), 10);
    let (code, text, _) = itse(&env, &work, &["install"], &[]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("\nfailed         rust-analyzer 2026-11-02 delegate_failed curl exit 22: curl: (22) The requested URL returned error: 403"), "{text}");
    assert!(
        !env.dir.join(format!("data/itse/dl/{}.part", "a".repeat(64))).exists() || fs::read(env.dir.join(format!("data/itse/dl/{}.part", "a".repeat(64)))).unwrap().is_empty(),
        "no error body is left in the part"
    );
}

#[test]
fn lock_refresh_writes_the_lock_from_recorded_json_and_status_lists_the_hosts() {
    let env = Env::new();
    let work = env.work();
    fs::create_dir_all(work.join(".git")).unwrap();
    assert_eq!(itse(&env, &work, &["use", "rust", "install=fetch"], &[]).0, Some(0));
    serve(&env, "api.github.com/repos/rust-lang/rust-analyzer/releases/latest", include_bytes!("../fixtures/pins/github-rust-analyzer-latest.json"));
    let (code, text, err) = itse(&env, &work, &["lock", "--refresh", "rust-analyzer"], &[]);
    assert_eq!(code, Some(0), "{text}{err}");
    assert!(text.starts_with(&format!("OK lock {} — 1 looked up for {}, 0 failed; nothing installed", work.display(), target())), "{text}");
    assert!(text.contains("\nchanged        rust-analyzer        none → 2026-11-02, sha256 4f8699eaee42…; next (agent): itse install rust-analyzer\n"), "{text}");
    let lock: Value = serde_json::from_slice(&fs::read(env.dir.join(format!("state/itse/repo/{}/itse.lock.json", rid(&env, &work)))).unwrap()).unwrap();
    assert_eq!(lock["packages"]["rust-analyzer"]["purl"], "pkg:github/rust-lang/rust-analyzer@2026-11-02");
    assert!(!env.dir.join("data/itse/store").exists(), "nothing installed");
    let (_, text, _) = itse(&env, &work, &["status", "--hosts"], &[]);
    assert!(text.contains("hosts: rust-analyzer 2026-11-02 from api.github.com, github.com, release-assets.githubusercontent.com"), "{text}");
}

#[test]
fn leftover_tmp_dirs_are_removed_when_the_daemon_starts() {
    let env = Env::new();
    let tmp = env.dir.join(format!("data/itse/store/sha256-{}.tmp", "c".repeat(64)));
    fs::create_dir_all(tmp.join("pkg/bin")).unwrap();
    fs::set_permissions(tmp.join("pkg"), fs::Permissions::from_mode(0o555)).unwrap();
    env.status_pid();
    assert!(!tmp.exists());
}

/// The pinned rust-analyzer .gz (about 15 MB) through the door: refreshed, verified, stored and
/// shimmed; installed again with the network cut; and a planted sha256 refused.
#[test]
#[ignore = "needs the door and api.github.com: ITSE_E2E_FETCH=1"]
fn the_real_rust_analyzer_is_fetched_through_the_door() {
    if std::env::var("ITSE_E2E_FETCH").as_deref() != Ok("1") {
        return eprintln!("skipped: set ITSE_E2E_FETCH=1");
    }
    let env = Env::new();
    let work = env.work();
    fs::create_dir_all(work.join(".git")).unwrap();
    let run = |args: &[&str], extra: &[(&str, &str)]| {
        let out = env.command(&work, args).envs(extra.iter().copied()).output().unwrap();
        (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
    };
    assert_eq!(run(&["use", "rust", "install=fetch"], &[]).0, Some(0));
    let (code, text) = run(&["lock", "--refresh", "rust-analyzer"], &[]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(run(&["trust"], &[]).0, Some(0));
    let started = Instant::now();
    let (code, text) = run(&["install"], &[]);
    assert_eq!(code, Some(0), "{text}");
    eprintln!("fetched in {:.1} s: {text}", started.elapsed().as_secs_f64());
    let shims = env.dir.join("data/itse/bin");
    let out = shim(&env, "rust-analyzer", &work, &format!("{}:/usr/bin:/bin", shims.display()));
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).starts_with("rust-analyzer "), "{}", stdout(&out));
    // Offline: the proxy is a closed port, so only the store can answer.
    fs::remove_file(shims.join("rust-analyzer")).unwrap();
    let offline = [("HTTPS_PROXY", "http://127.0.0.1:9"), ("https_proxy", "http://127.0.0.1:9")];
    let (code, text) = run(&["install", "rust-analyzer"], &offline);
    assert_eq!(code, Some(0), "{text}");
    assert!(shims.join("rust-analyzer").exists());
    // A planted sha256: the download is refused and deleted, and nothing enters the store.
    let path = env.dir.join(format!("state/itse/repo/{}/itse.lock.json", rid(&env, &work)));
    let mut lock: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let real = lock["packages"]["rust-analyzer"]["targets"][target()]["sha256"].as_str().unwrap().to_string();
    let planted = format!("{}{}", &real[..63], if real.ends_with('0') { '1' } else { '0' });
    lock["packages"]["rust-analyzer"]["targets"][target()]["sha256"] = planted.clone().into();
    fs::write(&path, lock.to_string()).unwrap();
    let (code, text) = run(&["install"], &[]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("sha256_mismatch"), "{text}");
    assert!(!env.dir.join(format!("data/itse/store/sha256-{planted}")).exists());
    assert!(!env.dir.join(format!("data/itse/dl/{planted}.part")).exists());
}
