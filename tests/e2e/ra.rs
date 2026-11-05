//! The real rust-analyzer end to end, run with the ignored tests (ITSE_E2E_RA=0 skips it), from PATH through the built-in
//! `rust` extension: nothing starts before trust, so no build script runs; then the cold index,
//! `problems` with the crate's type errors (an edit to a module no query opened included, and one
//! error that `cargo build` reports too), find, refs, outline and info by name, and the time a
//! post-edit hook call takes to see an edit; and the daemon's VmHWM on a copy of a larger crate,
//! ITSE_E2E_RA_CRATE=<dir> or this one.

use super::resolver::{repo, tree};
use super::*;

const DATA: &str = "pub struct Region;\n\nimpl Region {\n    pub fn save(&self) -> u32 {\n        1\n    }\n}\n\npub fn free_fn() -> Region {\n    Region\n}\n\n\
    pub struct SaveRequest;\n\npub fn total() -> u32 {\n    free_fn().save() + Region.save()\n}\n";
const UTIL: &str = "/// Broken on purpose.\npub fn helper() -> u32 {\n    let x: u32 = \"one\";\n    x\n}\n";
/// A generic impl, whose container rust-analyzer gives with its arguments, and an inline module,
/// whose items it gives no container.
const MORE: &str = "pub struct Wrapper<T>(pub T);\n\nimpl<T> Wrapper<T> {\n    pub fn wrap_new(t: T) -> Self {\n        Wrapper(t)\n    }\n}\n\n\
    pub mod inner {\n    pub fn deep_fn() -> u32 {\n        2\n    }\n}\n";
/// The module the hook timing edits, which no query opens.
const EXTRA: &str = "pub fn extra() -> u32 {\n    3\n}\n";

/// One real rust-analyzer at a time: the larger crate's index and cargo check load the machine
/// enough that the other one's checks run past its limits.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whenever the ignored tests run, as the opt-in gate runs them, unless ITSE_E2E_RA=0: a missing
/// rust-analyzer fails rather than passes unrun.
fn enabled() -> bool {
    let off = std::env::var("ITSE_E2E_RA").as_deref() == Ok("0");
    assert!(off || Command::new("rust-analyzer").arg("--version").output().is_ok_and(|o| o.status.success()), "rust-analyzer is not on PATH; ITSE_E2E_RA=0 skips this");
    if off {
        eprintln!("skipped: ITSE_E2E_RA=0");
    }
    !off
}

/// `itse ARGS` in the work folder, servers kept for 10 minutes: (exit code, stdout and stderr).
fn run(env: &Env, args: &[&str], extra: &[(&str, &str)]) -> (Option<i32>, String) {
    let out = env.command(&env.work(), args).env("ITSE_SERVER_IDLE_S", "600").envs(extra.iter().copied()).output().unwrap();
    (out.status.code(), stdout(&out) + &String::from_utf8_lossy(&out.stderr))
}

/// `itse ARGS` again until `done` holds for its text or `limit` passed: the last text and how long it took.
fn until(env: &Env, args: &[&str], extra: &[(&str, &str)], limit: Duration, done: impl Fn(&str) -> bool) -> (String, Duration) {
    let started = Instant::now();
    loop {
        let (_, text) = run(env, args, extra);
        if done(&text) || started.elapsed() >= limit {
            return (text, started.elapsed());
        }
        thread::sleep(Duration::from_millis(300));
    }
}

#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn the_real_rust_analyzer_waits_for_trust_then_reports_problems_and_resolves_names() {
    if !enabled() {
        return;
    }
    let _one = ONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = Env::new();
    let marker = env.dir.join("build-script-ran");
    let build = format!("fn main() {{\n    let _ = std::fs::write({:?}, \"ran\");\n    println!(\"cargo::rerun-if-changed=build.rs\");\n}}\n", marker.display().to_string());
    let files = [
        ("Cargo.toml", "[package]\nname = \"ra-ws\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n"),
        ("build.rs", build.as_str()),
        ("src/lib.rs", "pub mod data;\npub mod extra;\npub mod more;\npub mod util;\n"),
        ("src/data.rs", DATA),
        ("src/extra.rs", EXTRA),
        ("src/more.rs", MORE),
        ("src/util.rs", UTIL),
    ];
    assert!(repo(&env, &files), "git");
    let itse = |args: &[&str]| run(&env, args, &[]);
    assert_eq!(itse(&["use", "rust"]).0, Some(0));
    let (code, text) = itse(&["find", "save"]);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.starts_with("UNTRUSTED find save — rust-analyzer runs project code\n") && text.contains("next (human): itse trust"), "{text}");
    let (code, text) = itse(&["problems"]);
    assert!(
        code == Some(3) && text.starts_with("UNTRUSTED problems — rust-analyzer runs project code; 0 errors, 0 warnings\n") && text.contains("next (human): itse trust"),
        "{text}"
    );
    thread::sleep(Duration::from_secs(2));
    assert!(!marker.exists(), "no build script ran before trust");
    assert_eq!(itse(&["trust"]).0, Some(0));
    let before = tree(&env.work());
    let (_, first) = itse(&["find", "save"]);
    eprintln!("the first find:\n{first}");
    assert!(first.lines().next().unwrap().contains("; rust-analyzer index cold"), "{first}");
    assert!(first.contains("\nrust-analyzer may write Cargo.lock into the workspace (a declared output)\n"), "{first}");

    let (text, took) = until(&env, &["problems"], &[], Duration::from_secs(90), |t| t.contains(" src/util.rs:3:18 "));
    eprintln!("problems showed the type error after {:.1} s:\n{text}", took.as_secs_f64());
    assert!(text.starts_with("FAIL problems — ") && text.contains(" src/util.rs:3:18 E0308: mismatched types [rust-analyzer]\n"), "{text}");
    // A module no query opened: rust-analyzer hears of the edit from the disk sync.
    fs::write(env.work().join("src/data.rs"), format!("{DATA}\npub fn bad() -> u32 {{\n    \"two\"\n}}\n")).unwrap();
    let (text, took) = until(&env, &["problems", "src/data.rs"], &[], Duration::from_secs(90), |t| t.contains(" src/data.rs:20:5 "));
    eprintln!("problems showed the edit's error after {:.1} s:\n{text}", took.as_secs_f64());
    assert!(text.starts_with("FAIL problems src/data.rs — 1 error") && text.contains("\nE1 error src/data.rs:20:5 E0308: mismatched types [rust-analyzer]\n"), "{text}");
    fs::write(env.work().join("src/data.rs"), DATA).unwrap();
    // Until warm too: under load, the check of the revert can still be running when it shows empty.
    let cleared = |t: &str| t.starts_with("EMPTY problems src/data.rs — 0 errors, 0 warnings; rust-analyzer index warm");
    let (text, _) = until(&env, &["problems", "src/data.rs"], &[], Duration::from_secs(90), cleared);
    assert!(cleared(&text), "the revert cleared it: {text}");

    let (code, text) = itse(&["find", "data::Region.save"]);
    eprintln!("{text}");
    assert!(code == Some(0) && text.starts_with("OK find data::Region.save — 1 match; rust-analyzer index warm; q:"), "{text}");
    assert!(text.contains(" src/data.rs:4:12 Region::save\n      4 |     pub fn save(&self) -> u32 {\n"), "{text}");
    let (code, text) = itse(&["find", "data::free_fn"]);
    assert!(code == Some(0) && text.contains("\nS1 function src/data.rs:9:8 ") && text.contains("\n      9 | pub fn free_fn() -> Region {\n"), "{text}");
    // `sav` matches by prefix only, so without the kind SaveRequest would be one of the matches.
    let (code, text) = itse(&["find", "sav"]);
    assert!(code == Some(0) && text.contains(" SaveRequest\n"), "{text}");
    let (code, text) = itse(&["find", "sav", "--kind", "method"]);
    assert!(code == Some(0) && text.contains(" src/data.rs:4:12 "), "{text}");
    assert!(!text.contains("SaveRequest"), "a struct is not a method: {text}");
    let (code, text) = itse(&["find", "Wrapper.wrap_new"]);
    assert!(code == Some(0) && text.contains("\nS1 function src/more.rs:4:12 Wrapper<T>::wrap_new\n"), "a generic impl: {text}");
    for name in ["inner::deep_fn", "more::inner::deep_fn"] {
        let (code, text) = itse(&["find", name]);
        assert!(code == Some(0) && text.contains("\nS1 function src/more.rs:10:12 deep_fn\n"), "an inline module: {text}");
    }
    let (code, text) = itse(&["find", "data::deep_fn"]);
    assert_eq!(code, Some(1), "not in that module: {text}");
    let (code, text) = itse(&["outline", "src/data.rs"]);
    eprintln!("{text}");
    assert!(code == Some(0) && text.contains(" src/data.rs:1:12 Region\n") && text.contains(" src/data.rs:4:12   save\n"), "{text}");
    let (code, text) = itse(&["refs", "data::Region.save"]);
    eprintln!("{text}");
    assert!(code == Some(0) && text.starts_with("OK refs data::Region.save — 2 references in 1 file; rust-analyzer index warm; q:"), "{text}");
    assert!(text.contains("\nR1 ref src/data.rs:16:15\n") && text.contains("\nR2 ref src/data.rs:16:31\n     16 |     free_fn().save() + Region.save()\n"), "{text}");
    let (code, text) = itse(&["info", "data::Region.save"]);
    eprintln!("{text}");
    assert!(code == Some(0) && text.contains(" src/data.rs:4:12 Region::save\n") && text.contains("\n   pub fn save(&self) -> u32\n"), "{text}");

    let hook = hook_timing(&env);
    eprintln!(
        "post-edit hook call (problems --new) until it shows an edit to an unopened module: p50 {} ms, p95 {} ms over {} edits: {hook:?}",
        pct(&hook, 50),
        pct(&hook, 95),
        hook.len()
    );
    let line = json!({"gate": "ra-hook", "p50_ms": pct(&hook, 50), "p95_ms": pct(&hook, 95), "ms": hook});
    let _ = fs::create_dir_all("target/budget");
    let _ = fs::OpenOptions::new().create(true).append(true).open("target/budget/history.jsonl").and_then(|mut f| writeln!(f, "{line}"));

    assert!(wait_until(Duration::from_secs(60), || marker.exists()), "trusted, rust-analyzer runs the build script");
    let ws = fs::read_dir(env.dir.join("cache/itse/rust-analyzer")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(ws, 1, "its cargo check builds under the cache, one dir per workspace");
    let after = tree(&env.work());
    let mut changed: Vec<_> = after.iter().filter(|(p, m)| before.get(*p) != Some(m)).map(|(p, _)| p).chain(before.keys().filter(|p| !after.contains_key(*p))).collect();
    changed.retain(|p| !p.ends_with("src/data.rs") && !p.ends_with("src/extra.rs") && !p.ends_with("src"));
    assert_eq!(changed, [&env.work().join("Cargo.lock")], "written inside the workspace besides the test's own edits");

    // `cargo build` reports the type error too: one item, with both sources.
    assert_eq!(itse(&["build"]).0, Some(1));
    let (text, _) = until(&env, &["problems", "src/util.rs"], &[], Duration::from_secs(90), |t| t.contains("[rust-analyzer, rustc]"));
    eprintln!("{text}");
    assert!(text.starts_with("FAIL problems src/util.rs — 1 error, 0 warnings; ") && text.contains("\nE1 error src/util.rs:3:18 E0308: mismatched types"), "{text}");
    assert_eq!(itse(&["stop", "lsp:rust-analyzer"]).0, Some(0));
}

fn pct(v: &[u64], p: usize) -> u64 {
    let mut v = v.to_vec();
    v.sort_unstable();
    v[(v.len() * p).div_ceil(100).max(1) - 1]
}

/// The milliseconds from an edit to src/extra.rs, which no query opened, until a hook's
/// `problems --new` shows it, over 12 edits, broken and fixed in turn: the hook's real path, with
/// the disk sync, rust-analyzer's check on the save, and the settle.
fn hook_timing(env: &Env) -> Vec<u64> {
    super::cargo::hook_problems(env, "timing");
    let mut ms = Vec::new();
    for n in 1..=12 {
        // Each broken line is another, so its error is new to the baseline.
        let broken = n % 2 == 1;
        let body = if broken { format!("{EXTRA}\npub fn bad{n}() -> u32 {{\n    let v{n}: u32 = \"x\";\n    v{n}\n}}\n") } else { EXTRA.to_string() };
        fs::write(env.work().join("src/extra.rs"), body).unwrap();
        let edited = Instant::now();
        loop {
            let text = super::cargo::hook_problems(env, "timing");
            let head = text.lines().next().unwrap_or_default();
            let shown = if broken { text.contains("\nE1 error src/extra.rs:6:") } else { head.contains(" fixed") && !head.contains(" 0 fixed") };
            if shown {
                break;
            }
            assert!(edited.elapsed() < Duration::from_secs(60), "edit {n} not shown after 60 s: {text}");
        }
        ms.push(edited.elapsed().as_millis() as u64);
    }
    ms
}

/// On a copy of ITSE_E2E_RA_CRATE=<dir>, else of this crate (without `target/` and `.git`), with
/// cargo's registry index and downloads copied into the private HOME so its dependencies resolve
/// offline, `problems` until it settles (15 min at most), then find and refs; the daemon's and
/// rust-analyzer's VmHWM are printed and appended to target/budget/history.jsonl.
#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn rust_analyzer_on_a_larger_crate_records_the_daemon_vmhwm() {
    if !enabled() {
        return;
    }
    let _one = ONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let src = std::env::var_os("ITSE_E2E_RA_CRATE").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    let env = Env::new();
    let cp = |from: &Path, to: &Path| Command::new("cp").arg("-r").arg(from).arg(to).status().is_ok_and(|s| s.success());
    fs::create_dir_all(env.work()).unwrap();
    for entry in fs::read_dir(&src).unwrap().flatten().filter(|e| e.file_name() != "target" && e.file_name() != ".git") {
        assert!(cp(&entry.path(), &env.work().join(entry.file_name())), "{}", entry.path().display());
    }
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"));
    fs::create_dir_all(env.dir.join("home/.cargo/registry")).unwrap();
    for part in ["index", "cache"] {
        assert!(cp(&cargo_home.join("registry").join(part), &env.dir.join("home/.cargo/registry")), "cargo's registry {part}");
    }
    assert!(repo(&env, &[]), "git");
    let offline = [("CARGO_NET_OFFLINE", "true")];
    for args in [&["use", "rust"][..], &["trust"]] {
        assert_eq!(run(&env, args, &offline).0, Some(0), "{args:?}");
    }
    let (text, took) = until(&env, &["problems"], &offline, Duration::from_secs(900), |t| !t.contains(" not settled after "));
    eprintln!("problems after {:.0} s:\n{}", took.as_secs_f64(), text.lines().take(12).collect::<Vec<_>>().join("\n"));
    let (_, found) = run(&env, &["find", "main", "--kind", "function"], &offline);
    let (_, refs) = run(&env, &["refs", &format!("S{}", 1)], &offline);
    eprintln!("{}\n{}", found.lines().next().unwrap_or_default(), refs.lines().next().unwrap_or_default());
    let (_, status) = run(&env, &["status"], &offline);
    let log = fs::read_to_string(env.daemons()[0].0.join("servers/rust-analyzer.log")).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(15).collect();
    eprintln!(
        "{}
server log, newest first:
{}",
        status.lines().find(|l| l.contains("lsp:rust-analyzer")).unwrap_or_default(),
        tail.join("\n")
    );
    let kb =
        |pid: i64| fs::read_to_string(format!("/proc/{pid}/status")).ok()?.lines().find_map(|l| l.strip_prefix("VmHWM:")?.trim().strip_suffix(" kB")?.trim().parse::<u64>().ok());
    let daemon = env.daemon_json()["pid"].as_i64().unwrap();
    let procs: Value = serde_json::from_slice(&fs::read(env.daemons()[0].0.join("procs.json")).unwrap()).unwrap();
    let ra = procs.as_array().unwrap().iter().find(|p| p["handle"] == "lsp:rust-analyzer").and_then(|p| p["pid"].as_i64());
    let (daemon_kb, ra_kb) = (kb(daemon), ra.and_then(kb));
    let name = src.file_name().unwrap_or_default().to_string_lossy().into_owned();
    eprintln!("VmHWM on a copy of {name}: daemon {daemon_kb:?} kB, rust-analyzer {ra_kb:?} kB; settled after {:.0} s: {}", took.as_secs_f64(), !text.contains(" not settled "));
    let line = json!({"gate": "ra-crate-vmhwm", "crate": name, "daemon_vmhwm_kb": daemon_kb, "ra_vmhwm_kb": ra_kb, "problems_s": took.as_secs()});
    let _ = fs::create_dir_all("target/budget");
    let _ = fs::OpenOptions::new().create(true).append(true).open("target/budget/history.jsonl").and_then(|mut f| writeln!(f, "{line}"));
    assert!(daemon_kb.is_some_and(|kb| kb < 20 << 10), "the daemon stays under 20 MB: {daemon_kb:?} kB");
}
