use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::time::SystemTime;

use serde_json::json;

use super::*;
use crate::manifest::use_ext;
use crate::resolve::resolve;
use crate::results::{Outcome, Verdict};
use crate::rpc::Cancel;
use crate::store::tests::Fake;
use crate::sys::tests::TempDir;
use crate::verbs::{Args, Caller, store_verb};

/// Recorded through the door on 2026-11-05 (trimmed to the fields Itse reads and a few others).
const RUST_ANALYZER: &str = include_str!("../../tests/fixtures/pins/github-rust-analyzer-latest.json");
const TEMURIN: &str = include_str!("../../tests/fixtures/pins/github-temurin25-latest.json");
const KOTLIN_META: &str = include_str!("../../tests/fixtures/pins/openvsx-kotlin-server-linux-arm64-latest.json");
const KOTLIN_BUNDLE: &str = include_str!("../../tests/fixtures/pins/openvsx-kotlin-server-0.0.86-linux-arm64-server-bundle.json");

const RA_URL: &str = "https://api.github.com/repos/rust-lang/rust-analyzer/releases/latest";
const TEMURIN_URL: &str = "https://api.github.com/repos/adoptium/temurin25-binaries/releases/latest";
const KOTLIN_URL: &str = "https://open-vsx.org/api/JetBrains/kotlin-server/linux-arm64/latest";
const BUNDLE_URL: &str = "https://open-vsx.org/vscode/unpkg/JetBrains/kotlin-server/0.0.86+linux-arm64/extension/server-bundle.json";

fn recorded() -> Fake {
    let ok = |f: Fake, url: &str, body: &str| f.answer(url, 0, "200 200", body.as_bytes());
    ok(ok(ok(ok(Fake::default(), RA_URL, RUST_ANALYZER), TEMURIN_URL, TEMURIN), KOTLIN_URL, KOTLIN_META), BUNDLE_URL, KOTLIN_BUNDLE)
}

struct Fx {
    _tmp: TempDir,
    ws: Workspace,
    dirs: Dirs,
}

impl Fx {
    /// A git workspace using rust (fetched) and kotlin, which requires jdk.
    fn new() -> Self {
        let tmp = TempDir::new("itse-pins");
        let home: OsString = tmp.0.join("home").into();
        fs::create_dir_all(tmp.0.join("work/.git")).unwrap();
        let (ws, dirs) = (Workspace::find(Some(tmp.0.join("work")), &tmp.0).unwrap(), Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap());
        for (ext, rest) in [("rust", vec!["install=fetch".to_string()]), ("kotlin", vec![])] {
            use_ext(&ws, &dirs, &Args { target: Some(ext.into()), rest, ..Args::default() }, true).unwrap();
        }
        Self { _tmp: tmp, ws, dirs }
    }

    /// `itse lock [--refresh] [only] --target linux_arm64` as the daemon runs it.
    fn refresh(&self, d: &Fake, only: Option<&str>, all: bool) -> Outcome {
        let flags = [("target".to_string(), json!("linux_arm64"))].into_iter().chain(all.then(|| ("refresh".to_string(), json!(true)))).collect();
        let args = Args { target: only.map(String::from), flags, ..Args::default() };
        store_verb(&self.ws, &self.dirs, &Caller::default(), "lock", &args, &Cancel::default(), d)
    }

    fn lock(&self) -> serde_json::Value {
        lock::read(&lock::repo_dir(&self.dirs, &self.ws).join("itse.lock.json")).unwrap()
    }
}

fn rows(o: &Outcome) -> Vec<(String, String, String)> {
    o.states.iter().map(|s| (s.item.clone(), s.state.clone(), s.reason.clone().unwrap_or_default())).collect()
}

#[test]
fn a_github_release_pins_the_asset_digest() {
    let tmp = TempDir::new("itse-pins");
    let home: OsString = tmp.0.join("home").into();
    let dirs = Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap();
    let src = PinSource::GithubRelease { repo: "rust-lang/rust-analyzer".into(), assets: [("linux_x64".into(), "rust-analyzer-x86_64-unknown-linux-gnu.gz".into())].into() };
    let (version, published, pin) = lookup(&recorded(), &dirs, &src, "linux_x64").unwrap();
    assert_eq!((version.as_str(), published.as_str()), ("2026-11-02", "2026-11-02T06:53:51Z"));
    assert_eq!(pin.url, "https://github.com/rust-lang/rust-analyzer/releases/download/2026-11-02/rust-analyzer-x86_64-unknown-linux-gnu.gz");
    assert_eq!(
        (pin.sha256.as_str(), pin.size, pin.provenance.as_str()),
        ("5249da68c3f36bb77ea7d401824f5e8bd8d0a47a162daf5f72208eb7c1bf1500", Some(15_296_882), "upstream:github-digest")
    );
    let f = lookup(&recorded(), &dirs, &src, "linux_riscv").unwrap_err();
    assert_eq!(f.reason, "the pin source names no build for linux_riscv");
}

#[test]
fn a_glob_picks_the_one_temurin_jdk_archive() {
    assert!(glob("OpenJDK25U-jdk_aarch64_linux_hotspot_*.tar.gz", "OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz"));
    for other in [
        "OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz.json",
        "OpenJDK25U-jdk_aarch64_alpine-linux_hotspot_25.0.5_8.tar.gz",
        "OpenJDK25U-jre_aarch64_linux_hotspot_25.0.5_8.tar.gz",
    ] {
        assert!(!glob("OpenJDK25U-jdk_aarch64_linux_hotspot_*.tar.gz", other), "{other}");
    }
    assert!(glob("a*b*c", "aXbYc") && glob("*", "") && !glob("a*", "ba"));
}

#[test]
fn an_open_vsx_bundle_pins_the_server_archive() {
    let tmp = TempDir::new("itse-pins");
    let home: OsString = tmp.0.join("home").into();
    let dirs = Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap();
    let src = PinSource::OpenvsxBundle { extension: "JetBrains/kotlin-server".into(), platforms: [("linux_arm64".into(), "linux-arm64".into())].into() };
    let fake = recorded();
    let (version, published, pin) = lookup(&fake, &dirs, &src, "linux_arm64").unwrap();
    assert_eq!((version.as_str(), &published[..10]), ("271.2085.0", "2026-11-04"));
    assert_eq!(pin.url, "https://download.jetbrains.com/language-server/kotlin-server/271.2085.0/kotlin-server-271.2085.0-aarch64.tar.gz");
    assert_eq!((pin.sha256.as_str(), pin.size, pin.provenance.as_str()), ("877aee612a17dab48144466282a6414ec58ab428a10b97c7e13edd9b6b3dc96e", None, "upstream:openvsx-bundle"));
    assert_eq!(fake.curls().iter().map(|c| c.last().unwrap().clone()).collect::<Vec<_>>(), [KOTLIN_URL, BUNDLE_URL]);
}

#[test]
fn refresh_writes_a_lock_diff_and_installs_nothing() {
    let fx = Fx::new();
    let fake = recorded();
    let o = fx.refresh(&fake, None, true);
    assert_eq!(o.head(), format!("OK lock {} — 3 looked up for linux_arm64, 0 failed; nothing installed", fx.ws.root.display()));
    assert_eq!(
        rows(&o),
        [
            ("kotlin-lsp".into(), "changed".into(), "none → 271.2085.0, sha256 877aee612a17…; expires about 2026-12-04".into()),
            ("rust-analyzer".into(), "changed".into(), "none → 2026-11-02, sha256 4f8699eaee42…".into()),
            ("jdk".into(), "changed".into(), "none → jdk-25.0.5+8, sha256 bb5b2ded252f…".into()),
        ]
    );
    assert!(o.states.iter().all(|s| s.next == Some(vec!["install".into(), s.item.clone()])));
    let lock = fx.lock();
    assert_eq!(
        lock["packages"]["kotlin-lsp"],
        json!({"purl": "pkg:generic/jetbrains/kotlin-lsp@271.2085.0", "install": "fetch", "published": "2026-11-04", "expires_estimate": "2026-12-04", "extension": "kotlin@0.1.0",
            "targets": {"linux_arm64": {"url": "https://download.jetbrains.com/language-server/kotlin-server/271.2085.0/kotlin-server-271.2085.0-aarch64.tar.gz",
                "sha256": "877aee612a17dab48144466282a6414ec58ab428a10b97c7e13edd9b6b3dc96e", "size": null, "provenance": "upstream:openvsx-bundle"}}})
    );
    assert_eq!(
        lock["packages"]["jdk"]["targets"]["linux_arm64"]["url"],
        "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_aarch64_linux_hotspot_25.0.5_8.tar.gz"
    );
    assert!(!fx.dirs.data.join("store").exists(), "a refresh installs nothing");
    assert!(fake.curls().iter().all(|c| c.last().unwrap().starts_with("https://api.github.com/") || c.last().unwrap().starts_with("https://open-vsx.org/")));
    let again = fx.refresh(&recorded(), None, true);
    assert!(again.states.iter().all(|s| s.state == "unchanged" && s.next.is_none()), "{:?}", rows(&again));
}

#[test]
fn without_refresh_only_missing_pins_are_looked_up_and_another_target_adds_to_the_entry() {
    let fx = Fx::new();
    fx.refresh(&recorded(), Some("rust-analyzer"), true);
    let fake = recorded();
    let o = fx.refresh(&fake, None, false);
    assert_eq!(o.states.iter().map(|s| s.item.as_str()).collect::<Vec<_>>(), ["kotlin-lsp", "jdk"], "rust-analyzer has its pin");
    let r = resolve(&fx.ws, &fx.dirs, &BTreeMap::new(), SystemTime::now()).unwrap();
    refresh(&recorded(), &fx.dirs, &fx.ws, &r, Some("rust-analyzer"), "linux_x64", false).unwrap();
    let targets = fx.lock()["packages"]["rust-analyzer"]["targets"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(targets, ["linux_arm64", "linux_x64"]);
}

#[test]
fn a_refused_pin_source_is_a_blocked_row_that_names_the_host() {
    let fx = Fx::new();
    let fake = Fake::default().answer(RA_URL, 56, "403 000", b"");
    let o = fx.refresh(&fake, Some("rust-analyzer"), true);
    let s = &o.states[0];
    assert_eq!((s.state.as_str(), s.reason.as_deref(), s.actor.as_deref()), ("blocked", Some("blocked_by_door host api.github.com; actor halti-owner"), Some("halti-owner")));
    assert_eq!(s.hosts, ["api.github.com", "github.com", "release-assets.githubusercontent.com"]);
    assert_eq!((o.verdict, o.err), (Verdict::Blocked, Some("blocked_by_door")), "the same as an install the door refuses");
    assert!(fx.lock()["packages"].as_object().unwrap().is_empty());
    // The site's own 403 (a rate limit) is no door refusal: the agent's, a failed row.
    let fake = Fake::default().answer(RA_URL, 22, "200 403", b"{\"message\": \"API rate limit exceeded\"}");
    let o = fx.refresh(&fake, Some("rust-analyzer"), true);
    assert_eq!((o.verdict, o.states[0].state.as_str(), o.states[0].actor.as_deref()), (Verdict::Fail, "failed", Some("agent")));
}

#[test]
fn what_upstream_sends_is_checked_before_it_reaches_the_lock() {
    let fx = Fx::new();
    let bundle = |body: &str| Fake::default().answer(KOTLIN_URL, 0, "200 200", KOTLIN_META.as_bytes()).answer(BUNDLE_URL, 0, "200 200", body.as_bytes());
    for body in [
        r#"{"version": "1", "url": "https://x.test/a.tar.gz"}"#,
        r#"{"version": "1", "url": "http://x.test/a", "sha256": "877aee612a17dab48144466282a6414ec58ab428a10b97c7e13edd9b6b3dc96e"}"#,
    ] {
        let o = fx.refresh(&bundle(body), Some("kotlin-lsp"), true);
        assert_eq!((o.verdict, o.states[0].state.as_str()), (Verdict::Fail, "failed"), "{body}");
        assert!(o.states[0].reason.as_deref().unwrap().starts_with("the pin source sent no https url, sha256 or version for linux_arm64: "), "{:?}", o.states[0]);
    }
    let digest = RUST_ANALYZER.replace("sha256:4f8699eaee4231df5a5ffd477106ecae3bf6328918a60de742798912ecff6869", "md5:4f86");
    let o = fx.refresh(&Fake::default().answer(RA_URL, 0, "200 200", digest.as_bytes()), Some("rust-analyzer"), true);
    assert_eq!(
        o.states[0].reason.as_deref(),
        Some(
            "the pin source sent no https url, sha256 or version for linux_arm64: url \"https://github.com/rust-lang/rust-analyzer/releases/download/2026-11-02/rust-analyzer-aarch64-unknown-linux-gnu.gz\", sha256 \"md5:4f86\""
        )
    );
    assert!(fx.lock()["packages"].as_object().unwrap().is_empty(), "nothing unchecked is written");
}

#[test]
fn expiry_is_counted_from_the_publish_date() {
    assert_eq!(add_days("2026-11-04T09:57:36.057595Z", 30).as_deref(), Some("2026-12-04"));
    assert_eq!(add_days("2024-02-28", 1).as_deref(), Some("2024-02-29"));
    assert_eq!(add_days("2026-12-31", 1).as_deref(), Some("2027-01-01"));
    assert_eq!(add_days("2000-03-01", 0).as_deref(), Some("2000-03-01"));
    assert_eq!(add_days("soon", 3), None);
}
