use std::sync::Mutex;

use super::*;
use crate::store::Out;
use crate::sys::tests::TempDir;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

const PROXY: &str = "-Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=3128";

#[test]
fn env_composition_appends_java_tool_options_and_sets_lang_only_when_unset() {
    let base = map(&[("PATH", "/usr/bin:/bin"), ("JAVA_TOOL_OPTIONS", PROXY), ("HOME", "/home/agent"), ("IJ_JAVA_OPTIONS", "-Xss4m")]);
    let (bins, entry) = (["/s/jdk/bin", "/s/kotlin/bin"].map(PathBuf::from), [("JAVA_HOME".to_string(), "/s/jdk".to_string())]);
    let jvm = [("IJ_JAVA_OPTIONS".to_string(), vec!["-Xmx2g".to_string()]), ("JAVA_TOOL_OPTIONS".to_string(), vec!["-XX:+UseSerialGC".to_string()])];
    let env = compose(base, &bins, &entry, &trust_flags(Path::new("/st/door-ab.p12")), &jvm);
    let tool = format!("{PROXY} -Djavax.net.ssl.trustStore=/st/door-ab.p12 -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=changeit -XX:+UseSerialGC");
    let want = map(&[
        ("PATH", "/s/jdk/bin:/s/kotlin/bin:/usr/bin:/bin"),
        ("JAVA_TOOL_OPTIONS", &tool),
        ("HOME", "/home/agent"),
        ("IJ_JAVA_OPTIONS", "-Xss4m -Xmx2g"),
        ("JAVA_HOME", "/s/jdk"),
        ("LANG", "C.UTF-8"),
    ]);
    assert_eq!(env, want);
    let kept = compose(map(&[("LANG", "fi_FI.UTF-8")]), &[], &[], &[], &[]);
    assert_eq!(kept, map(&[("PATH", ""), ("LANG", "fi_FI.UTF-8")]));
    let lc_all = compose(map(&[("PATH", "/bin"), ("LC_ALL", "C")]), &[], &[], &[], &[("X_OPTS".into(), vec![])]);
    assert_eq!(lc_all, map(&[("PATH", "/bin"), ("LC_ALL", "C")]), "no LANG beside LC_ALL, and no empty variable");
}

#[test]
fn flags_already_present_are_skipped() {
    let base = map(&[("PATH", "/bin"), ("JAVA_TOOL_OPTIONS", "-Djavax.net.ssl.trustStore=/halti/cacerts -Xmx2g")]);
    let env = compose(base, &[], &[], &trust_flags(Path::new("/st/door.p12")), &[("JAVA_TOOL_OPTIONS".into(), vec!["-Xmx2g".into()])]);
    assert_eq!(env["JAVA_TOOL_OPTIONS"], "-Djavax.net.ssl.trustStore=/halti/cacerts -Xmx2g -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=changeit");
}

#[test]
fn keytool_argv_is_exact() {
    let argv = keytool_args(Path::new("/usr/local/share/ca-certificates/door.crt"), Path::new("/s/jvm/door-ab.p12"));
    let want =
        ["-importcert", "-noprompt", "-file", "/usr/local/share/ca-certificates/door.crt", "-storetype", "PKCS12", "-storepass", "changeit", "-keystore", "/s/jvm/door-ab.p12"];
    assert_eq!(argv, want);
}

/// A keytool that writes its `-keystore` file, and records each call.
#[derive(Default)]
struct Keytool(Mutex<Vec<Vec<String>>>);

impl Delegate for Keytool {
    fn run(&self, tool: &str, args: &[String], _: Option<&Path>) -> Result<Out> {
        self.0.lock().unwrap().push([tool.to_string()].into_iter().chain(args.iter().cloned()).collect());
        fs::write(&args[9], "p12")?;
        Ok(Out::default())
    }
}

#[test]
fn the_truststore_is_built_once_per_ca_by_the_jdks_keytool() {
    let tmp = TempDir::new("itse-env");
    let home: OsString = tmp.0.join("home").into();
    let dirs = Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap();
    let ca = tmp.0.join("door.crt");
    fs::write(&ca, "-----BEGIN CERTIFICATE-----\nA\n").unwrap();
    let keytool = Keytool::default();
    let store = truststore(&keytool, &dirs, Path::new("/s/jdk/bin/keytool"), &ca).unwrap();
    let name = format!("door-{}.p12", &sys::sha256_hex(&fs::read(&ca).unwrap())[..16]);
    assert_eq!(store, dirs.state.join("jvm").join(&name));
    assert_eq!(fs::read_to_string(&store).unwrap(), "p12");
    let calls = keytool.0.lock().unwrap().clone();
    assert_eq!(calls[0][0], "/s/jdk/bin/keytool");
    let tmp_store = dirs.state.join("jvm").join(format!(".{name}.{}.tmp", std::process::id()));
    assert_eq!(calls[0][1..], keytool_args(&ca, &tmp_store), "a temporary name of this process's own");
    assert_eq!(fs::read_dir(dirs.state.join("jvm")).unwrap().count(), 1, "the temporary file is renamed into place");
    assert_eq!(truststore(&keytool, &dirs, Path::new("/s/jdk/bin/keytool"), &ca).unwrap(), store);
    assert_eq!(keytool.0.lock().unwrap().len(), 1, "built once");
    fs::write(&ca, "-----BEGIN CERTIFICATE-----\nB\n").unwrap();
    assert_ne!(truststore(&keytool, &dirs, Path::new("/s/jdk/bin/keytool"), &ca).unwrap(), store, "a new CA, a new store");
}

#[test]
fn shims_point_at_the_cli_path_and_bad_names_are_refused() {
    let tmp = TempDir::new("itse-env");
    let home: OsString = tmp.0.join("home").into();
    let dirs = Dirs::from_env(|k| (k == "HOME").then(|| home.clone())).unwrap();
    let names = ["java".to_string(), "javac".to_string()];
    link_shims(&dirs, names.iter(), Path::new("/opt/itse-old/itse")).unwrap();
    link_shims(&dirs, names.iter(), Path::new("/home/agent/.cargo/bin/itse")).unwrap();
    for name in &names {
        assert_eq!(fs::read_link(dirs.data.join("bin").join(name)).unwrap(), Path::new("/home/agent/.cargo/bin/itse"));
    }
    assert_eq!(fs::read_dir(dirs.data.join("bin")).unwrap().count(), 2, "no temporary link is left");
    for bad in ["../java", ".hidden", "itse"] {
        assert_eq!(link_shims(&dirs, [bad.to_string()].iter(), Path::new("/x/itse")).unwrap_err().to_string(), format!("bin {bad} can't be a shim"));
    }
    assert!(link_shims(&dirs, names.iter(), Path::new("itse")).is_err(), "a relative exe path");
}
