use super::*;

#[test]
fn a_full_lock_parses_and_round_trips() {
    let text = r#"{ "itse_lock": 1,
  "packages": { "kotlin-lsp": { "purl": "pkg:generic/jetbrains/kotlin-lsp@271.2085.0", "install": "fetch",
  "targets": { "linux_arm64": { "url": "https://download.jetbrains.com/language-server/kotlin-server/271.2085.0/kotlin-server-271.2085.0-aarch64.tar.gz",
                                "sha256": "877aee61…dc96e", "size": null, "provenance": "upstream:openvsx-bundle" } },
  "published": "2026-11-04", "expires_estimate": "2026-12-04", "extension": "kotlin@0.1.0" } },
  "project_pins": { "gradle-wrapper": { "distributionSha256Sum": "present" } } }"#;
    let lock: Lock = serde_json::from_str(text).unwrap();
    assert_eq!(lock.packages["kotlin-lsp"].targets["linux_arm64"].provenance, "upstream:openvsx-bundle");
    let again: Lock = serde_json::from_str(&serde_json::to_string(&lock).unwrap()).unwrap();
    assert_eq!(again.project_pins, lock.project_pins);
    assert!(serde_json::from_str::<Pin>(r#"{"url": "u", "sha256": "s", "mirror": "m"}"#).is_err());
}

#[test]
fn trust_consent_and_found_records_round_trip() {
    let trust = Trust { repo: "/work/crm/.git".into(), accepted: vec!["ab".into()], actor: "tty".into(), at: "2026-11-05T10:00:00.000Z".into() };
    let back: Trust = serde_json::from_str(&serde_json::to_string(&trust).unwrap()).unwrap();
    assert_eq!((back.repo, back.accepted, back.actor), (trust.repo, trust.accepted, trust.actor));
    let consents: BTreeMap<String, Consent> = serde_json::from_str(r#"{"kotlin-lsp": {"eula_hash": "0123456789abcdef", "actor": "tty", "at": "t"}}"#).unwrap();
    assert_eq!(consents["kotlin-lsp"].eula_hash.as_deref(), Some("0123456789abcdef"));
    let found: Found = serde_json::from_str(r#"{"path": "/usr/bin/node", "version": "22.1.0", "sha256": "aa"}"#).unwrap();
    assert_eq!(found.version.as_deref(), Some("22.1.0"));
}

fn fixture(tmp: &crate::sys::tests::TempDir) -> (Workspace, Dirs) {
    let vars: BTreeMap<&str, std::ffi::OsString> = [("HOME", tmp.0.join("home").into_os_string())].into();
    fs::create_dir_all(tmp.0.join("work/.git")).unwrap();
    (Workspace::find(Some(tmp.0.join("work")), &tmp.0).unwrap(), Dirs::from_env(|k| vars.get(k).cloned()).unwrap())
}

#[test]
fn trust_keeps_the_last_8_accepted_hashes_and_records_the_actor() {
    let tmp = crate::sys::tests::TempDir::new("itse-lock");
    let (ws, dirs) = fixture(&tmp);
    let hashes: Vec<String> = (0..10).map(|i| format!("{i:064x}")).collect();
    for h in &hashes {
        trust(&dirs, &ws, h, false, "non-tty").unwrap();
    }
    trust(&dirs, &ws, &hashes[5], false, "tty").unwrap();
    let t: Trust = read(&repo_dir(&dirs, &ws).join("trust.json")).unwrap();
    assert_eq!(t.accepted, [2, 3, 4, 6, 7, 8, 9, 5].map(|i| hashes[i].clone()), "re-accepting moves a hash to the end");
    assert_eq!((t.actor.as_str(), t.repo.as_str()), ("tty", ws.repo.to_str().unwrap()));
    assert!(trusted(&dirs, &ws, &hashes[2]).unwrap() && !trusted(&dirs, &ws, &hashes[1]).unwrap());
    let o = trust(&dirs, &ws, &hashes[9], true, "tty").unwrap();
    assert_eq!((o.head(), o.states[0].state.as_str()), (format!("OK trust {} — trust revoked", ws.root.display()), "untrusted"));
    assert!(!trusted(&dirs, &ws, &hashes[2]).unwrap());
}

#[test]
fn trust_lives_in_state_keyed_by_the_repo_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = crate::sys::tests::TempDir::new("itse-lock");
    let (ws, dirs) = fixture(&tmp);
    trust(&dirs, &ws, &"a".repeat(64), false, "tty").unwrap();
    let path = dirs.state.join("repo").join(&ws.rid).join("trust.json");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_dir(&ws.root).unwrap().count(), 1, "only .git: nothing written into the workspace");
    fs::write(&path, "{").unwrap();
    assert!(format!("{:#}", trusted(&dirs, &ws, "a").unwrap_err()).starts_with(&format!("reading {}", path.display())));
}

#[test]
fn a_found_binary_is_hashed_again_only_when_it_changes() {
    let tmp = crate::sys::tests::TempDir::new("itse-lock");
    let (_, dirs) = fixture(&tmp);
    let bin = tmp.0.join("node");
    fs::write(&bin, "v1").unwrap();
    assert_eq!(found(&dirs, "node", &bin).unwrap().sha256, crate::sys::sha256_hex(b"v1"));
    let file = dirs.state.join("found.json");
    let mut all: BTreeMap<String, Found> = read(&file).unwrap();
    all.get_mut("node").unwrap().sha256 = "cached".into();
    write(&file, &all).unwrap();
    assert_eq!(found(&dirs, "node", &bin).unwrap().sha256, "cached", "same path, size and mtime");
    fs::write(&bin, "v22").unwrap();
    assert_eq!(found(&dirs, "node", &bin).unwrap().sha256, crate::sys::sha256_hex(b"v22"));
}

#[test]
fn a_hash_consent_counts_only_for_the_current_file() {
    let tmp = crate::sys::tests::TempDir::new("itse-lock");
    fs::write(tmp.0.join("EULA.txt"), "v1").unwrap();
    let specs: Vec<ConsentSpec> = serde_json::from_str(r#"[{"id": "eula", "kind": "hash", "of_file": "EULA.txt", "hex_prefix": 16}, {"id": "region", "kind": "choice"}]"#).unwrap();
    let hash = file_hash(&tmp.0, &specs[0]).unwrap();
    assert_eq!(hash, crate::sys::sha256_hex(b"v1")[..16]);
    let all: BTreeMap<String, Consent> = [("p".to_string(), Consent { eula_hash: Some(hash), region: Some("eu".into()), ..Consent::default() })].into();
    assert!(missing_consents(&all, "p", &specs, Some(&tmp.0)).is_empty());
    assert_eq!(missing_consents(&all, "p", &specs, None), ["eula"], "without the package the EULA can't be checked");
    assert_eq!(missing_consents(&all, "q", &specs, Some(&tmp.0)), ["eula", "region"]);
    fs::write(tmp.0.join("EULA.txt"), "v2").unwrap();
    assert_eq!(missing_consents(&all, "p", &specs, Some(&tmp.0)), ["eula"]);
}
