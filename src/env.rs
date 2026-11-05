//! The environment Itse gives what it starts (design §4.6): composed in a fixed order on top of
//! the caller's env, never a cleared one (Halti's box rules), with the JVM truststore for the
//! door's CA. `itse exec`, `itse env` and shims get it now; servers get the same composition when
//! they are spawned (S3/S4).
//!
//! Shims are symlinks in `~/.local/share/itse/bin/` named after a package's bins, pointing at the
//! CLI that installed them; `itse` started under another name runs as that tool's shim.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};

use crate::resolve::{Resolved, resolve_for, which};
use crate::results::State;
use crate::store::{Delegate, Real, row_of};
use crate::sys;
use crate::verbs::Args;
use crate::workspace::{Dirs, Workspace};

/// Compose an env (§4.6), in order: the base; PATH with the package bin folders in front; the
/// entry's own env; the truststore flags appended to JAVA_TOOL_OPTIONS after what is there (Halti's
/// proxy flags), skipping flags already set; each package's jvm_args appended to its variable;
/// `LANG=C.UTF-8` only when neither LANG nor LC_ALL is set.
pub fn compose(mut env: BTreeMap<String, String>, bins: &[PathBuf], entry: &[(String, String)], trust: &[String], jvm: &[(String, Vec<String>)]) -> BTreeMap<String, String> {
    let path: Vec<String> = bins.iter().map(|b| b.display().to_string()).chain(env.get("PATH").cloned()).collect();
    env.insert("PATH".into(), path.join(":"));
    env.extend(entry.iter().cloned());
    for (var, flags) in [("JAVA_TOOL_OPTIONS".to_string(), trust.to_vec())].iter().chain(jvm) {
        let old = env.get(var).cloned().unwrap_or_default();
        let key = |f: &str| f.split('=').next().unwrap_or_default().to_string();
        let added: Vec<&str> = flags.iter().map(String::as_str).filter(|f| !old.split_whitespace().any(|o| key(o) == key(f))).collect();
        let joined = [old.trim(), &added.join(" ")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        env.extend(Some((var.clone(), joined)).filter(|v| !v.1.is_empty()));
    }
    let lang = !env.contains_key("LANG") && !env.contains_key("LC_ALL");
    env.extend(lang.then(|| ("LANG".to_string(), "C.UTF-8".to_string())));
    env
}

/// `keytool -importcert -noprompt -file <ca> -storetype PKCS12 -storepass changeit -keystore <store>`.
pub fn keytool_args(ca: &Path, store: &Path) -> Vec<String> {
    let (ca, store) = (ca.display().to_string(), store.display().to_string());
    ["-importcert", "-noprompt", "-file", &ca, "-storetype", "PKCS12", "-storepass", "changeit", "-keystore", &store].map(String::from).to_vec()
}

/// What every JVM Itse starts or shims gets. The password is not a secret.
pub fn trust_flags(store: &Path) -> Vec<String> {
    let flags = [format!("-Djavax.net.ssl.trustStore={}", store.display()), "-Djavax.net.ssl.trustStoreType=PKCS12".into()];
    flags.into_iter().chain(["-Djavax.net.ssl.trustStorePassword=changeit".to_string()]).collect()
}

/// A user-space JDK lacks the door's CA: `state/jvm/door-<sha>.p12` holds it, built once by that
/// JDK's own keytool and again when the CA file's hash changes.
pub fn truststore(d: &dyn Delegate, dirs: &Dirs, keytool: &Path, ca: &Path) -> Result<PathBuf> {
    let sha = sys::sha256_hex(&fs::read(ca).with_context(|| format!("reading the door CA {}", ca.display()))?);
    let (dir, name) = (dirs.state.join("jvm"), format!("door-{}.p12", &sha[..16]));
    if dir.join(&name).is_file() {
        return Ok(dir.join(name));
    }
    sys::private_dir(&dir)?;
    // Each process builds its own copy, so two first JVMs don't import into one file; the last rename wins.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let out = d.run(&keytool.display().to_string(), &keytool_args(ca, &tmp), None)?;
    ensure!(out.code == 0, "keytool exit {}: {}", out.code, out.stderr.trim());
    fs::rename(&tmp, dir.join(&name)).context("keeping the truststore")?;
    Ok(dir.join(name))
}

fn usable(s: &State) -> bool {
    matches!(s.state.as_str(), "ready" | "unpinned" | "expiring")
}

/// The env of `itse env`, `itse exec` and shims: `base` with the bin folders of the usable store
/// packages in use, their jvm_args, and the truststore when a package brings a keytool and the
/// box names its CA in NODE_EXTRA_CA_CERTS.
pub fn workspace_env(r: &Resolved, dirs: &Dirs, base: BTreeMap<String, String>, d: &dyn Delegate) -> Result<BTreeMap<String, String>> {
    let store = dirs.data.join("store");
    let (mut bins, mut jvm, mut keytool) = (BTreeSet::new(), Vec::new(), None);
    for (name, root) in r.roots.iter().filter(|(n, root)| root.starts_with(&store) && row_of(&r.states, n).is_some_and(usable)) {
        let Some(pkg) = r.manifest.package(name) else { continue };
        for (bin, rel) in &pkg.bin {
            let dir = root.join(rel).parent().map_or(root.clone(), Path::to_path_buf);
            keytool = if bin == "keytool" { Some(root.join(rel)) } else { keytool };
            bins.insert(dir);
        }
        jvm.push((pkg.jvm_args_env.clone().unwrap_or_else(|| "JAVA_TOOL_OPTIONS".into()), pkg.jvm_args.clone()));
    }
    let store = keytool.zip(base.get("NODE_EXTRA_CA_CERTS")).map(|(keytool, ca)| truststore(d, dirs, &keytool, Path::new(ca))).transpose()?;
    Ok(compose(base, &bins.into_iter().collect::<Vec<_>>(), &[], &store.map_or(Vec::new(), |s| trust_flags(&s)), &jvm))
}

/// `~/.local/share/itse/bin/<name>` → `exe`, the calling CLI's own path (sent in `hello`), never
/// the daemon's current_exe, which may name a deleted file (§0.2). A link is replaced atomically.
pub fn link_shims<'a>(dirs: &Dirs, names: impl Iterator<Item = &'a String>, exe: &Path) -> Result<()> {
    let dir = dirs.data.join("bin");
    ensure!(exe.is_absolute(), "the CLI sent no absolute exe_path, so no shim is linked");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for name in names {
        ensure!(!name.contains('/') && !name.starts_with('.') && !name.starts_with("itse"), "bin {name} can't be a shim");
        if fs::read_link(dir.join(name)).is_ok_and(|to| to == exe) {
            continue;
        }
        // Each process builds its own copy, so two first JVMs don't import into one file; the last rename wins.
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(exe, &tmp).and_then(|()| fs::rename(&tmp, dir.join(name))).with_context(|| format!("linking the shim {name}"))?;
    }
    Ok(())
}

/// `itse` run as `name` (a shim): find the workspace from the cwd, compose its env and exec the
/// package's binary. A tool the workspace doesn't manage runs the next `name` on PATH past the
/// shims; a managed one that is missing, untrusted, expired or lacks consent prints one line and
/// exits 127.
pub fn shim(name: &str, args: Vec<OsString>) -> Result<u8> {
    let dirs = Dirs::from_env(|k| std::env::var_os(k))?;
    let shims = dirs.data.join("bin");
    // The shims leave PATH, so neither the resolver nor the tool finds one again.
    let mut base: BTreeMap<String, String> = std::env::vars().collect();
    let path = base.get("PATH").map(|p| p.split(':').filter(|d| Path::new(d) != shims).collect::<Vec<_>>().join(":")).unwrap_or_default();
    base.insert("PATH".into(), path);
    let given = std::env::var_os("ITSE_WORKSPACE").filter(|v| !v.is_empty()).map(PathBuf::from);
    let (ws, cwd) = (Workspace::find(given, &std::env::current_dir()?)?, std::env::current_dir()?);
    let r = resolve_for(&ws, &dirs, &base, SystemTime::now(), false)?;
    let uses = r.manifest.uses();
    let managed = uses.iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| &e.0.packages).find_map(|(p, pkg)| Some((p, pkg, pkg.bin.get(name)?)));
    let program = match managed {
        Some((p, pkg, rel)) => match (r.roots.get(p), row_of(&r.states, p)) {
            // A path install runs what PATH finds by that name; its root need not hold `rel`.
            (Some(root), Some(s)) if usable(s) && (r.trusted || !pkg.executes_project_code) => match root.starts_with(dirs.data.join("store")) {
                false => which(name, base.get("PATH")).unwrap_or_else(|| root.join(rel)),
                true => root.join(rel),
            },
            (_, s) => {
                // A usable package that is refused waits for trust; anything else, for itself.
                let s = if s.is_some_and(usable) { row_of(&r.states, "workspace") } else { s }.cloned().unwrap_or_default();
                let hosts = if s.hosts.is_empty() { String::new() } else { format!(" (needs hosts {})", s.hosts.join(", ")) };
                let next = s.next.map_or(String::new(), |n| format!("; run 'itse {}'", n.join(" ")));
                eprintln!("itse: {} {} ({}){next}{hosts}", s.item, s.state, s.reason.unwrap_or_default());
                return Ok(127);
            }
        },
        None => match which(name, base.get("PATH")).filter(|p| fs::canonicalize(p).ok() != fs::canonicalize("/proc/self/exe").ok()) {
            Some(next) => next,
            None => {
                eprintln!("itse: {name} is not managed in {} and not on PATH", cwd.display());
                return Ok(127);
            }
        },
    };
    let env = workspace_env(&r, &dirs, base.clone(), &Real(base))?;
    let e = Command::new(&program).args(args).envs(&env).exec();
    bail!("running {}: {e}", program.display())
}

/// `itse env [--sh]` prints what the composed env adds or changes (`--sh`: `export` lines to eval);
/// `itse exec -- cmd…` runs a command in it. Both run in the CLI, without the daemon.
pub fn cli(ws: &Workspace, dirs: &Dirs, args: &Args, exec: bool) -> Result<u8> {
    let base: BTreeMap<String, String> = std::env::vars().collect();
    let r = resolve_for(ws, dirs, &base, SystemTime::now(), false)?;
    let env = workspace_env(&r, dirs, base.clone(), &Real(base.clone()))?;
    if exec {
        let Some((program, rest)) = args.rest.split_first() else { bail!("missing argument <cmd>") };
        let e = Command::new(program).args(rest).envs(&env).exec();
        bail!("running {program}: {e}");
    }
    for (k, v) in env.iter().filter(|(k, v)| base.get(*k) != Some(*v)) {
        println!("{}", if args.flags.contains_key("sh") { format!("export {k}='{}'", v.replace('\'', "'\\''")) } else { format!("{k}={v}") });
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
