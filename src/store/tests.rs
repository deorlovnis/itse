use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{MetadataExt, symlink};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::*;
use crate::results::Verdict;
use crate::rpc::Cancel;
use crate::sys::tests::TempDir;
use crate::verbs::{Args, Caller, store_verb};

/// One answer of the fake curl: exit, `-w` codes, the bytes it writes.
type Answer = (i32, &'static str, Vec<u8>);

/// A fake curl and the real tar, gzip and python3: each URL answers in turn with `(exit, -w codes,
/// bytes)`; bytes are written to `-o` like curl does (appended after a part when resuming), an
/// error's body too, as `--fail-with-body` keeps it. Every call is recorded with whether its `-o`
/// file existed then.
#[derive(Default)]
pub struct Fake {
    pub answers: Mutex<BTreeMap<String, VecDeque<Answer>>>,
    pub calls: Mutex<Vec<(Vec<String>, bool)>>,
}

impl Fake {
    pub fn answer(self, url: &str, code: i32, w: &'static str, bytes: &[u8]) -> Self {
        self.answers.lock().unwrap().entry(url.into()).or_default().push_back((code, w, bytes.to_vec()));
        self
    }

    pub fn curls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().iter().filter(|c| c.0[0] == "curl").map(|c| c.0.clone()).collect()
    }
}

impl Delegate for Fake {
    fn run(&self, tool: &str, args: &[String], stdout: Option<&Path>) -> Result<Out> {
        let out = args.iter().position(|a| a == "-o").map(|i| PathBuf::from(&args[i + 1]));
        let existed = out.as_ref().is_some_and(|o| o.exists());
        self.calls.lock().unwrap().push(([tool.to_string()].into_iter().chain(args.iter().cloned()).collect(), existed));
        if tool != "curl" {
            return Real(std::env::vars().collect()).run(tool, args, stdout);
        }
        let url = args.last().cloned().unwrap_or_default();
        let (code, w, bytes) = self.answers.lock().unwrap().get_mut(&url).and_then(VecDeque::pop_front).unwrap_or((6, "000 000", vec![]));
        if code == 0 || !bytes.is_empty() {
            let resume = args.iter().any(|a| a == "-C");
            let mut file = fs::OpenOptions::new().create(true).write(true).append(resume).truncate(!resume).open(out.unwrap())?;
            io::Write::write_all(&mut file, &bytes)?;
        }
        let stderr = if code == 0 { String::new() } else { format!("  % Total\ncurl: ({code}) the fake says no\n") };
        Ok(Out { code, stdout: w.into(), stderr })
    }
}

struct Fx {
    tmp: TempDir,
    dirs: Dirs,
}

impl Fx {
    fn new() -> Self {
        let tmp = TempDir::new("itse-store");
        let home: OsString = tmp.0.join("home").into();
        let dirs = Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap();
        Self { tmp, dirs }
    }

    /// A tar.gz of the folder `src` (made by `make` under `tmp/src`), its bytes.
    fn tar(&self, make: impl Fn(&Path)) -> Vec<u8> {
        let src = self.tmp.0.join("src");
        let _ = fs::remove_dir_all(&src);
        fs::create_dir_all(src.join("pkg/bin")).unwrap();
        fs::write(src.join("pkg/bin/tool"), "#!/bin/sh\necho tool\n").unwrap();
        make(&src.join("pkg"));
        let out = self.tmp.0.join("a.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&src).arg("pkg").status().unwrap().success());
        fs::read(out).unwrap()
    }

    fn part(&self, sha: &str) -> PathBuf {
        self.dirs.data.join("dl").join(format!("{sha}.part"))
    }
}

fn gz(text: &str) -> Vec<u8> {
    let mut child = Command::new("gzip").arg("-c").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
    child.wait_with_output().unwrap().stdout
}

fn package(archive: &str, strip: u32, bin: &str) -> Package {
    Package { archive: Some(archive.into()), strip, bin: [(bin.rsplit('/').next().unwrap().to_string(), bin.to_string())].into(), ..Package::default() }
}

fn pin(url: &str, bytes: &[u8]) -> Pin {
    Pin { url: url.into(), sha256: sys::sha256_hex(bytes), size: Some(bytes.len() as u64), provenance: "upstream:github-digest".into() }
}

const URL: &str = "https://files.test/pkg";

#[test]
fn curl_and_tar_argv_are_exact() {
    let (head, w) = (["--fail-with-body", "-sSL", "--proto", "=https", "--proto-redir", "=https", "-w"], "%{http_connect} %{http_code} %{url_effective}");
    let argv = curl_args("https://github.com/x.gz", Path::new("/d/abc.part"), true);
    assert_eq!(argv, [&head[..], &[w, "-C", "-", "-o", "/d/abc.part", "https://github.com/x.gz"]].concat());
    assert_eq!(curl_args("https://a.test/b", Path::new("/o"), false), [&head[..], &[w, "-o", "/o", "https://a.test/b"]].concat());
    assert_eq!(tar_args(Path::new("/d/x.part"), Path::new("/s/x.tmp")), ["--no-same-owner", "-x", "-f", "/d/x.part", "-C", "/s/x.tmp"]);
}

#[test]
fn a_gz_package_is_verified_unpacked_read_only_and_reused() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\necho analyzer\n");
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let (pkg, pin) = (package("gz", 0, "rust-analyzer"), pin(URL, &bytes));
    let dir = install(&fake, &fx.dirs, &pkg, &pin).unwrap();
    assert_eq!(dir, fx.dirs.data.join(format!("store/sha256-{}", pin.sha256)));
    let bin = fs::metadata(dir.join("rust-analyzer")).unwrap();
    assert_eq!((bin.mode() & 0o777, fs::metadata(&dir).unwrap().mode() & 0o777), (0o555, 0o555), "read-only and executable");
    assert_eq!(fs::read_to_string(dir.join("rust-analyzer")).unwrap(), "#!/bin/sh\necho analyzer\n");
    assert!(!fx.part(&pin.sha256).exists(), "the download goes once it is in the store");
    let names: Vec<String> = entries(&fx.dirs.data.join("store")).map(|e| e.0).collect();
    assert_eq!(names, [format!("sha256-{}", pin.sha256)], "no .tmp is left");
    let calls = fake.calls.lock().unwrap().iter().map(|c| c.0[0].clone()).collect::<Vec<_>>();
    assert_eq!(calls, ["curl", "gzip"]);
    assert_eq!(fake.calls.lock().unwrap()[1].0[1..], ["-dc".to_string(), fx.part(&pin.sha256).display().to_string()]);
    assert_eq!(install(&fake, &fx.dirs, &pkg, &pin).unwrap(), dir);
    assert_eq!(fake.calls.lock().unwrap().len(), 2, "the store's copy is used without any tool");
}

#[test]
fn a_sha256_mismatch_deletes_the_download() {
    let fx = Fx::new();
    let fake = Fake::default().answer(URL, 0, "200 200", b"not what was pinned");
    let pin = Pin { sha256: "ab".repeat(32), ..pin(URL, b"x") };
    let f = install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap_err();
    assert!(f.reason.starts_with(&format!("sha256_mismatch: {URL} has {}", sys::sha256_hex(b"not what was pinned"))), "{}", f.reason);
    assert_eq!((f.actor, f.row("tool").state.as_str()), ("agent", "failed"));
    assert!(!fx.part(&pin.sha256).exists());
    assert!(!fx.dirs.data.join(format!("store/sha256-{}", pin.sha256)).exists());
}

#[test]
fn a_lock_sha256_that_is_not_hex_is_refused_before_any_path_is_made() {
    let fx = Fx::new();
    let pin = Pin { sha256: "../../etc".into(), ..pin(URL, b"x") };
    let f = install(&Fake::default(), &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap_err();
    assert_eq!(f.reason, "the lock's sha256 \"../../etc\" is not 64 lowercase hex digits");
}

#[test]
fn a_complete_part_file_is_verified_without_curl() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\n");
    let pin = pin(URL, &bytes);
    fs::create_dir_all(fx.dirs.data.join("dl")).unwrap();
    fs::write(fx.part(&pin.sha256), &bytes).unwrap();
    let fake = Fake::default();
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap();
    assert!(fake.curls().is_empty(), "{:?}", fake.calls.lock().unwrap());
}

#[test]
fn a_complete_but_wrong_part_is_fetched_again() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\n");
    let pin = pin(URL, &bytes);
    fs::create_dir_all(fx.dirs.data.join("dl")).unwrap();
    fs::write(fx.part(&pin.sha256), vec![b'x'; bytes.len()]).unwrap();
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap();
    assert!(!fake.calls.lock().unwrap()[0].1, "the bad part was deleted before curl ran");
}

#[test]
fn curl_exit_33_retries_without_resume() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\n");
    let pin = pin(URL, &bytes);
    fs::create_dir_all(fx.dirs.data.join("dl")).unwrap();
    fs::write(fx.part(&pin.sha256), b"half").unwrap();
    let fake = Fake::default().answer(URL, 33, "200 200", b"").answer(URL, 0, "200 200", &bytes);
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap();
    let curls = fake.curls();
    assert_eq!(curls.len(), 2);
    assert!(curls[0].windows(2).any(|w| w == ["-C", "-"]) && !curls[1].contains(&"-C".to_string()), "{curls:?}");
    let existed: Vec<bool> = fake.calls.lock().unwrap().iter().filter(|c| c.0[0] == "curl").map(|c| c.1).collect();
    assert_eq!(existed, [true, false], "the part is deleted before the fresh try");
}

#[test]
fn an_http_416_retries_once_without_resume_and_then_fails() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\n");
    let fake = Fake::default().answer(URL, 22, "200 416", b"").answer(URL, 0, "200 200", &bytes);
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &Pin { size: None, ..pin(URL, &bytes) }).unwrap();
    assert_eq!(fake.curls().len(), 2);
    for code in [(22, "200 416"), (33, "200 200")] {
        let fake = Fake::default().answer(URL, code.0, code.1, b"").answer(URL, code.0, code.1, b"");
        let f = install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin(URL, b"other")).unwrap_err();
        let reason = format!("delegate_failed curl exit {}: % Total | curl: ({}) the fake says no", code.0, code.0);
        assert_eq!((fake.curls().len(), f.reason.as_str()), (2, reason.as_str()), "a fresh try is not retried again");
    }
}

#[test]
fn an_error_body_is_cut_off_the_part_so_a_resume_stays_whole() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\necho whole\n");
    let pin = pin(URL, &bytes);
    fs::create_dir_all(fx.dirs.data.join("dl")).unwrap();
    fs::write(fx.part(&pin.sha256), &bytes[..10]).unwrap();
    let fake = Fake::default().answer(URL, 22, "200 503", b"<html>busy</html>").answer(URL, 0, "200 200", &bytes[10..]);
    let f = install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap_err();
    assert_eq!((f.actor, fs::read(fx.part(&pin.sha256)).unwrap()), ("agent", bytes[..10].to_vec()));
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap();
}

#[test]
fn a_dropped_connection_keeps_what_arrived_for_the_resume() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\necho resumed\n");
    let pin = pin(URL, &bytes);
    let fake = Fake::default().answer(URL, 18, "200 200", &bytes[..12]).answer(URL, 0, "200 200", &bytes[12..]);
    assert!(install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap_err().reason.starts_with("delegate_failed curl exit 18: "));
    assert_eq!(fs::read(fx.part(&pin.sha256)).unwrap(), bytes[..12]);
    install(&fake, &fx.dirs, &package("gz", 0, "tool"), &pin).unwrap();
}

#[test]
fn blocked_by_door_names_the_host_and_actor() {
    let fx = Fx::new();
    let halti = b"halti: the site compressed its answer, so halti can't mask it\n";
    let cases = [
        (56, "403 000", &b""[..], "blocked_by_door host github.com; actor halti-owner"),
        (56, "502 000", b"", "door_upstream_tls_failed host github.com; actor halti-owner"),
        (22, "200 403", halti, "door_refused_write host github.com; actor halti-owner"),
        (22, "200 421", halti, "door_host_mismatch host github.com; actor halti-owner"),
        (22, "200 502", halti, "door_masked_encoding host github.com; actor halti-owner"),
        (56, "403 000 https://release-assets.githubusercontent.com/x?sig=1", b"", "blocked_by_door host release-assets.githubusercontent.com; actor halti-owner"),
    ];
    let url = "https://github.com:443/rust-lang/x.gz";
    for (code, w, body, reason) in cases {
        let f = install(&Fake::default().answer(url, code, w, body), &fx.dirs, &package("gz", 0, "tool"), &pin(url, b"x")).unwrap_err();
        assert_eq!((f.reason.as_str(), f.actor), (reason, "halti-owner"));
        let row = f.row("rust-analyzer 2026-11-02");
        assert_eq!((row.state.as_str(), row.actor.as_deref()), ("blocked", Some("halti-owner")));
    }
    let f = install(&Fake::default().answer(url, 6, "000 000", b""), &fx.dirs, &package("gz", 0, "tool"), &pin(url, b"x")).unwrap_err();
    assert_eq!((f.reason.as_str(), f.actor), ("delegate_failed curl exit 6: % Total | curl: (6) the fake says no", "agent"));
    let f = install(&Fake::default().answer(url, 22, "000 403", b""), &fx.dirs, &package("gz", 0, "tool"), &pin(url, b"x")).unwrap_err();
    assert_eq!(f.actor, "agent", "a 403 without the door's tunnel is the site's");
    for (w, body) in [("200 403", &b"{\"message\": \"API rate limit exceeded\"}"[..]), ("200 502", b"<html>bad gateway</html>"), ("200 421", b"")] {
        let f = install(&Fake::default().answer(url, 22, w, body), &fx.dirs, &package("gz", 0, "tool"), &pin(url, b"x")).unwrap_err();
        assert_eq!(f.actor, "agent", "the site's own {w} inside the tunnel is no door refusal");
    }
}

#[test]
fn a_tar_package_is_stripped_and_its_internal_links_kept() {
    let fx = Fx::new();
    let bytes = fx.tar(|pkg| symlink("tool", pkg.join("bin/alias")).unwrap());
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let dir = install(&fake, &fx.dirs, &package("tar.gz", 1, "bin/tool"), &pin(URL, &bytes)).unwrap();
    assert_eq!(fs::read_to_string(dir.join("bin/alias")).unwrap(), "#!/bin/sh\necho tool\n");
    assert_eq!(fs::metadata(dir.join("bin")).unwrap().mode() & 0o222, 0);
    let tar = fake.calls.lock().unwrap().iter().find(|c| c.0[0] == "tar").unwrap().0.clone();
    assert_eq!(tar[1..4], ["--no-same-owner", "-x", "-f"]);
    assert_eq!(entries(&fx.dirs.data.join("store")).count(), 1, "the .tmp left after the strip is gone");
}

#[test]
fn an_absolute_symlink_in_an_archive_is_refused() {
    let fx = Fx::new();
    let bytes = fx.tar(|pkg| symlink("/etc/passwd", pkg.join("bin/passwd")).unwrap());
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let f = install(&fake, &fx.dirs, &package("tar.gz", 1, "bin/tool"), &pin(URL, &bytes)).unwrap_err();
    assert_eq!(f.reason, "bin/passwd links to /etc/passwd, outside the package; refused");
    assert_eq!(entries(&fx.dirs.data.join("store")).count(), 0, "neither the package nor its .tmp is left");
}

#[test]
fn links_that_lead_out_of_the_tree_are_refused_lexically_or_through_a_chain() {
    let fx = Fx::new();
    for (make, reason) in [
        (Box::new(|p: &Path| symlink("../../x", p.join("bin/up")).unwrap()) as Box<dyn Fn(&Path)>, "bin/up links to ../../x, outside the package; refused"),
        (Box::new(|p: &Path| symlink("..", p.join("bin/up")).and_then(|()| symlink("bin/up/..", p.join("out"))).unwrap()), "out links to bin/up/.., outside the package; refused"),
    ] {
        let bytes = fx.tar(make);
        let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
        assert_eq!(install(&fake, &fx.dirs, &package("tar.gz", 1, "bin/tool"), &pin(URL, &bytes)).unwrap_err().reason, reason);
    }
}

#[test]
fn zst_and_unknown_archives_are_refused_and_a_failing_tool_is_named() {
    let fx = Fx::new();
    let fake = Fake::default().answer(URL, 0, "200 200", b"zz");
    let f = install(&fake, &fx.dirs, &package("tar.zst", 0, "tool"), &pin(URL, b"zz")).unwrap_err();
    assert_eq!(f.reason, "archive kind tar.zst is refused; tar.gz, tar.xz, gz, zip and vsix are known");
    let fake = Fake::default().answer(URL, 0, "200 200", b"not a tar");
    let f = install(&fake, &fx.dirs, &package("tar.gz", 0, "tool"), &pin(URL, b"not a tar")).unwrap_err();
    assert!(f.reason.starts_with("delegate_failed tar exit 2: "), "{}", f.reason);
    let bytes = gz("x");
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let f = install(&fake, &fx.dirs, &package("gz", 0, "../escaped"), &pin(URL, &bytes)).unwrap_err();
    assert_eq!(f.reason, "bin ../escaped is not a file in the package");
    assert!(!fx.dirs.data.join("store/escaped").exists(), "nothing is written by a bad bin's name");
}

#[test]
fn a_gz_bin_in_a_folder_gets_its_folder() {
    let fx = Fx::new();
    let bytes = gz("#!/bin/sh\n");
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let dir = install(&fake, &fx.dirs, &package("gz", 0, "bin/tool"), &pin(URL, &bytes)).unwrap();
    assert_eq!(fs::read_to_string(dir.join("bin/tool")).unwrap(), "#!/bin/sh\n");
}

#[test]
fn gc_removes_read_only_packages_no_lock_pins() {
    let fx = Fx::new();
    let (a, b) = (gz("#!/bin/sh\na\n"), gz("#!/bin/sh\nb\n"));
    let fake = Fake::default().answer("https://f.test/a", 0, "200 200", &a).answer("https://f.test/b", 0, "200 200", &b);
    let kept = install(&fake, &fx.dirs, &package("gz", 0, "a"), &pin("https://f.test/a", &a)).unwrap();
    let gone = install(&fake, &fx.dirs, &package("gz", 0, "b"), &pin("https://f.test/b", &b)).unwrap();
    let lock = serde_json::json!({"itse_lock": 1, "packages": {"a": {"targets": {"linux_arm64": {"url": "https://f.test/a", "sha256": sys::sha256_hex(&a)}}}}});
    lock::write(&fx.dirs.state.join("repo/0123456789ab/itse.lock.json"), &lock).unwrap();
    fs::write(fx.dirs.data.join(format!("dl/{}.part", sys::sha256_hex(b"stale"))), b"st").unwrap();
    let (dl, held) = (fx.dirs.data.join("dl"), sys::sha256_hex(b"held"));
    let guard = File::create(dl.join(format!("{held}.lock"))).unwrap();
    guard.lock().unwrap();
    let (mut removed, b_lock) = (vec![], format!("{}.lock", sys::sha256_hex(&b)));
    let gone_too = soon(|| {
        removed.extend(gc(&fx.dirs).unwrap());
        removed.contains(&b_lock)
    });
    assert!(gone_too, "{removed:?}");
    assert_eq!(removed.len(), 3, "{removed:?}");
    assert!(kept.is_dir() && !gone.exists());
    assert!(removed.contains(&format!("sha256-{}", sys::sha256_hex(&b))) && removed.contains(&b_lock), "its install lock goes too");
    assert!(dl.join(format!("{held}.lock")).exists() && dl.join(format!("{}.lock", sys::sha256_hex(&a))).exists(), "a held or pinned install lock stays");
}

#[test]
fn gc_removes_nothing_while_a_lock_cannot_be_read() {
    let fx = Fx::new();
    let a = gz("#!/bin/sh\na\n");
    let kept = install(&Fake::default().answer(URL, 0, "200 200", &a), &fx.dirs, &package("gz", 0, "a"), &pin(URL, &a)).unwrap();
    let lock = serde_json::json!({"itse_lock": 1, "from_a_newer_itse": true, "packages": {"a": {"targets": {"linux_arm64": {"url": URL, "sha256": sys::sha256_hex(&a)}}}}});
    let path = fx.dirs.state.join("repo/0123456789ab/itse.lock.json");
    lock::write(&path, &lock).unwrap();
    let e = gc(&fx.dirs).unwrap_err();
    assert!(format!("{e:#}").starts_with(&format!("gc removes nothing while a lock can't be read: reading {}: unknown field", path.display())), "{e:#}");
    assert!(kept.is_dir());
}

#[test]
fn leftover_tmp_dirs_are_removed_at_start_unless_an_install_holds_them() {
    let fx = Fx::new();
    let (store, dl) = (fx.dirs.data.join("store"), fx.dirs.data.join("dl"));
    let (dead, live) = (store.join(format!("sha256-{}.tmp", "a".repeat(64))), store.join(format!("sha256-{}.tmp", "b".repeat(64))));
    for dir in [&dead, &live] {
        fs::create_dir_all(dir.join("pkg/bin")).unwrap();
        fs::set_permissions(dir.join("pkg"), Permissions::from_mode(0o555)).unwrap();
    }
    fs::create_dir_all(&dl).unwrap();
    let held = File::create(dl.join(format!("{}.lock", "b".repeat(64)))).unwrap();
    held.lock().unwrap();
    clean(&fx.dirs);
    assert!(!dead.exists() && live.exists());
    drop(held);
    assert!(soon(|| {
        clean(&fx.dirs);
        !live.exists()
    }));
}

/// `done`, tried again for up to 2 s: a test elsewhere in this process that forks while a lock here
/// is held gives the child a copy of it until the child's exec, so for that moment a released lock
/// looks held, as it may to a daemon's gc or start, which then leaves its file for the next one.
fn soon(mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    while !done() {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn a_pin_source_document_is_read_through_curl_and_deleted() {
    let fx = Fx::new();
    let fake = Fake::default().answer("https://api.github.com/x", 0, "200 200", br#"{"tag_name": "v1"}"#);
    assert_eq!(get_json(&fake, &fx.dirs, "https://api.github.com/x").unwrap()["tag_name"], "v1");
    assert_eq!(entries(&fx.dirs.data.join("dl")).count(), 0);
    let fake = Fake::default().answer("https://api.github.com/x", 0, "200 200", b"<html>");
    assert_eq!(get_json(&fake, &fx.dirs, "https://api.github.com/x").unwrap_err().reason, "https://api.github.com/x is not JSON: expected value at line 1 column 1");
}

#[test]
fn the_row_of_a_package_matches_its_name_and_version_only() {
    let rows: Vec<State> = ["jdk-extra 1", "jdk 25.0.5+8", "cargo (project)"].map(|i| State { item: i.into(), ..State::default() }).to_vec();
    assert_eq!(row_of(&rows, "jdk").unwrap().item, "jdk 25.0.5+8");
    assert!(row_of(&rows, "cargo").is_none() && row_of(&rows, "jd").is_none());
}

#[test]
fn install_goes_on_past_a_package_whose_shim_cannot_be_linked_and_stops_on_cancel() {
    let fx = Fx::new();
    fs::create_dir_all(fx.tmp.0.join("work/.git")).unwrap();
    let ws = crate::workspace::Workspace::find(Some(fx.tmp.0.join("work")), &fx.tmp.0).unwrap();
    let args = Args { target: Some("rust".into()), rest: vec!["install=fetch".into()], ..Args::default() };
    crate::manifest::use_ext(&ws, &fx.dirs, &args, true).unwrap();
    let bytes = gz("#!/bin/sh\n");
    let lock = serde_json::json!({"itse_lock": 1, "packages": {"rust-analyzer": {"purl": "pkg:github/rust-lang/rust-analyzer@1", "targets": {crate::resolve::target(): {"url": URL, "sha256": sys::sha256_hex(&bytes)}}}}});
    lock::write(&lock::repo_dir(&fx.dirs, &ws).join("itse.lock.json"), &lock).unwrap();
    let fake = Fake::default().answer(URL, 0, "200 200", &bytes);
    let cancel = Cancel::default();
    // A caller that sent no exe_path: the package is stored, its row says why there is no shim.
    let o = store_verb(&ws, &fx.dirs, &Caller::default(), "install", &Args::default(), &cancel, &fake);
    assert_eq!((o.verdict, o.err), (Verdict::Fail, Some("io")), "{}", o.head());
    let row = &o.states[0];
    assert_eq!((row.item.as_str(), row.state.as_str(), row.reason.as_deref()), ("rust-analyzer 1", "failed", Some("the CLI sent no absolute exe_path, so no shim is linked")));
    assert!(fx.dirs.data.join(format!("store/sha256-{}", sys::sha256_hex(&bytes))).is_dir());
    cancel.cancel();
    let o = store_verb(&ws, &fx.dirs, &Caller::default(), "install", &Args::default(), &cancel, &fake);
    assert_eq!((o.verdict, o.err), (Verdict::Error, Some("cancel")), "{}", o.head());
}
