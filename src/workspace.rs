//! The workspace: its root, the repo it belongs to, its ids, and where its state and socket live
//! (design §3.1). Everything here reads files; no git process runs.
//!
//! A worktree or a submodule has a `.git` file, so each is its own workspace (its own daemon,
//! servers and results), while all worktrees of one clone share a repo key, and with it the
//! developer layer, trust and lock.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::sys;

/// sun_path holds 108 bytes, the last one a NUL.
const MAX_SOCKET_PATH: usize = 107;

pub struct Workspace {
    /// Canonical: `--root`, else ITSE_WORKSPACE, else the nearest ancestor with `.git`, else the cwd.
    pub root: PathBuf,
    pub git: bool,
    /// The canonical git common dir, shared by every worktree of a clone; the root without git.
    pub repo: PathBuf,
    /// The first 12 hex digits of sha256(root) and of sha256(repo).
    pub ws: String,
    pub rid: String,
}

impl Workspace {
    pub fn find(given: Option<PathBuf>, cwd: &Path) -> Result<Self> {
        let start = given.as_deref().unwrap_or(cwd);
        let start = fs::canonicalize(start).with_context(|| format!("workspace {}", start.display()))?;
        let root = match given {
            Some(_) => start,
            None => start.ancestors().find(|d| d.join(".git").exists()).unwrap_or(&start).to_path_buf(),
        };
        let git = root.join(".git").exists();
        let repo = if git { repo_key(&root)? } else { root.clone() };
        Ok(Self { ws: id(&root), rid: id(&repo), root, git, repo })
    }

    /// What people call the repo: the clone's folder for a standard `.git`, else the root.
    pub fn repo_name(&self) -> &Path {
        match self.repo.parent() {
            Some(parent) if self.repo.ends_with(".git") => parent,
            _ => &self.root,
        }
    }

    pub fn state_dir(&self, dirs: &Dirs) -> PathBuf {
        dirs.state.join("ws").join(&self.ws)
    }

    /// `ws/<ws>/itse.sock` under the state dir, whatever XDG_RUNTIME_DIR says, so a shell and a
    /// harness find one daemon; `<tmp>/itse-<uid>/<ws>.sock` only when that would overflow sun_path.
    /// `tmp` is `/tmp` outside tests, never TMPDIR, which differs between a shell and a harness.
    pub fn socket(&self, dirs: &Dirs, tmp: &Path) -> Result<PathBuf> {
        let sock = self.state_dir(dirs).join("itse.sock");
        if sock.as_os_str().len() <= MAX_SOCKET_PATH {
            return Ok(sock);
        }
        let dir = tmp.join(format!("itse-{}", sys::uid()));
        let sock = dir.join(format!("{}.sock", self.ws));
        if sock.as_os_str().len() > MAX_SOCKET_PATH {
            bail!("socket path {} is longer than {MAX_SOCKET_PATH} bytes", sock.display());
        }
        let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
        let meta = fs::symlink_metadata(&dir).with_context(|| format!("creating {}", dir.display()))?;
        if !meta.is_dir() || meta.uid() != sys::uid() || meta.mode() & 0o077 != 0 {
            bail!("socket dir {} is not private (owner {}, mode {:o}); remove it", dir.display(), meta.uid(), meta.mode() & 0o777);
        }
        Ok(sock)
    }
}

/// The git common dir, from files: a `.git` dir is its own; a `.git` file names a gitdir, whose
/// `commondir` file (a worktree) points at the common dir, and without one (a submodule) it is.
fn repo_key(root: &Path) -> Result<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Ok(fs::canonicalize(dot)?);
    }
    let text = fs::read_to_string(&dot).with_context(|| format!("reading {}", dot.display()))?;
    let Some(gitdir) = text.trim().strip_prefix("gitdir:").map(|d| root.join(d.trim())) else { bail!("{} has no gitdir line", dot.display()) };
    let common = match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => gitdir.join(common.trim()),
        Err(_) => gitdir,
    };
    fs::canonicalize(&common).with_context(|| format!("git dir {} named by {}", common.display(), dot.display()))
}

fn id(path: &Path) -> String {
    sys::sha256_hex(path.as_os_str().as_encoded_bytes())[..12].to_string()
}

/// The four folders of decision 8, each ending in `itse`.
#[allow(dead_code)] // config, data and cache are read from S1 on
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    /// From `XDG_*_HOME`, else the usual folders under HOME. `var` is `std::env::var_os` outside tests.
    /// Every dir is absolute: a relative one would land in the client repo, the daemon's cwd.
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let abs = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = abs("HOME");
        let dir = |name: &str, default: &str| match (abs(name), &home) {
            (Some(dir), _) => Ok(dir.join("itse")),
            (None, Some(home)) => Ok(home.join(default).join("itse")),
            (None, None) => Err(anyhow!("HOME is not an absolute path and {name} is unset; itse keeps its state under them")),
        };
        Ok(Self {
            config: dir("XDG_CONFIG_HOME", ".config")?,
            data: dir("XDG_DATA_HOME", ".local/share")?,
            state: dir("XDG_STATE_HOME", ".local/state")?,
            cache: dir("XDG_CACHE_HOME", ".cache")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;
    use crate::sys::tests::TempDir;

    /// git with a private home and config, or None when git isn't on PATH.
    fn git(dir: &Path, home: &Path, args: &[&str]) -> Option<String> {
        let mut cmd = Command::new("git");
        // A hook running check.sh exports GIT_DIR or GIT_INDEX_FILE; they must not reach the temp repos.
        for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
            cmd.env_remove(key);
        }
        let out = cmd
            .args(["-c", "user.name=itse", "-c", "user.email=itse@example.invalid", "-c", "init.defaultBranch=main", "-c", "protocol.file.allow=always"])
            .args(args)
            .current_dir(dir)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .ok()?;
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Some(String::from_utf8(out.stdout).unwrap().trim().to_string())
    }

    /// A repo with one commit at `<tmp>/main`, or None (and a note) without git.
    fn repo(tmp: &TempDir) -> Option<PathBuf> {
        let main = tmp.0.join("main");
        fs::create_dir_all(main.join("src")).unwrap();
        fs::write(main.join("src/lib.rs"), "").unwrap();
        let Some(_) = git(&tmp.0, &tmp.0, &["init", "-q", "main"]) else {
            eprintln!("skipped: needs git");
            return None;
        };
        git(&main, &tmp.0, &["add", "."]);
        git(&main, &tmp.0, &["commit", "-qm", "base"]);
        Some(main)
    }

    fn common_dir(dir: &Path, home: &Path) -> PathBuf {
        fs::canonicalize(dir.join(git(dir, home, &["rev-parse", "--git-common-dir"]).unwrap())).unwrap()
    }

    #[test]
    fn the_workspace_of_a_worktree_is_the_worktree() {
        let tmp = TempDir::new("itse-ws");
        let Some(main) = repo(&tmp) else { return };
        git(&main, &tmp.0, &["worktree", "add", "-q", "../wt"]);
        let ws = Workspace::find(None, &tmp.0.join("wt/src")).unwrap();
        assert_eq!(ws.root, tmp.0.join("wt"));
        assert!(ws.git);
        assert_ne!(ws.ws, Workspace::find(None, &main).unwrap().ws, "the worktree gets its own daemon");
    }

    #[test]
    fn a_worktree_and_its_main_checkout_share_a_repo_key() {
        let tmp = TempDir::new("itse-ws");
        let Some(main) = repo(&tmp) else { return };
        git(&main, &tmp.0, &["worktree", "add", "-q", "../wt"]);
        let (a, b) = (Workspace::find(None, &main).unwrap(), Workspace::find(None, &tmp.0.join("wt")).unwrap());
        assert_eq!((&a.repo, &a.rid), (&b.repo, &b.rid));
        assert_eq!(a.repo, common_dir(&main, &tmp.0));
        assert_eq!(b.repo, common_dir(&tmp.0.join("wt"), &tmp.0));
        assert_eq!((a.repo_name(), b.repo_name()), (main.as_path(), main.as_path()));
    }

    #[test]
    fn a_submodule_has_its_own_repo_key() {
        let tmp = TempDir::new("itse-ws");
        let Some(main) = repo(&tmp) else { return };
        let lib = tmp.0.join("lib");
        fs::create_dir(&lib).unwrap();
        git(&lib, &tmp.0, &["init", "-q"]);
        git(&lib, &tmp.0, &["commit", "-q", "--allow-empty", "-m", "lib"]);
        git(&main, &tmp.0, &["submodule", "add", "-q", lib.to_str().unwrap(), "sub"]);
        let (outer, sub) = (Workspace::find(None, &main).unwrap(), Workspace::find(None, &main.join("sub")).unwrap());
        assert_eq!(sub.root, main.join("sub"));
        assert_eq!(sub.repo, common_dir(&main.join("sub"), &tmp.0));
        assert_eq!(sub.repo, main.join(".git/modules/sub"));
        assert_ne!(sub.rid, outer.rid);
        assert_eq!(sub.repo_name(), main.join("sub"));
    }

    #[test]
    fn the_nearest_git_ancestor_is_the_root_and_without_git_the_cwd_is() {
        let tmp = TempDir::new("itse-ws");
        fs::create_dir_all(tmp.0.join("outer/.git")).unwrap();
        fs::create_dir_all(tmp.0.join("outer/inner/.git")).unwrap();
        fs::create_dir_all(tmp.0.join("outer/inner/a/b")).unwrap();
        assert_eq!(Workspace::find(None, &tmp.0.join("outer/inner/a/b")).unwrap().root, tmp.0.join("outer/inner"));
        let plain = Workspace::find(None, &tmp.0).unwrap();
        assert_eq!((plain.git, &plain.repo, plain.repo_name()), (false, &tmp.0, tmp.0.as_path()));
        let given = Workspace::find(Some(tmp.0.join("outer/inner/a")), &tmp.0).unwrap();
        assert_eq!(given.root, tmp.0.join("outer/inner/a"), "--root is taken as given");
    }

    #[test]
    fn ids_are_stable_and_12_hex_digits() {
        let tmp = TempDir::new("itse-ws");
        fs::create_dir(tmp.0.join(".git")).unwrap();
        let (a, b) = (Workspace::find(None, &tmp.0).unwrap(), Workspace::find(Some(tmp.0.join(".")), &tmp.0).unwrap());
        assert_eq!((&a.ws, &a.rid), (&b.ws, &b.rid));
        assert_eq!(a.ws, sys::sha256_hex(tmp.0.as_os_str().as_encoded_bytes())[..12]);
        assert!(a.ws.len() == 12 && a.ws.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.ws, a.rid);
    }

    #[test]
    fn a_gitdir_line_that_is_missing_is_an_error_naming_the_file() {
        let tmp = TempDir::new("itse-ws");
        fs::write(tmp.0.join(".git"), "nonsense\n").unwrap();
        let err = Workspace::find(None, &tmp.0).err().unwrap();
        assert!(format!("{err:#}").contains("/.git has no gitdir line"), "{err:#}");
    }

    fn dirs(vars: &[(&str, &str)]) -> Dirs {
        let vars: BTreeMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        Dirs::from_env(|k| vars.get(k).cloned()).unwrap()
    }

    #[test]
    fn dirs_follow_xdg_and_fall_back_to_home() {
        let d = dirs(&[("HOME", "/home/a"), ("XDG_STATE_HOME", "/s"), ("XDG_CACHE_HOME", "relative")]);
        assert_eq!(d.state, Path::new("/s/itse"));
        assert_eq!(d.config, Path::new("/home/a/.config/itse"));
        assert_eq!(d.data, Path::new("/home/a/.local/share/itse"));
        assert_eq!(d.cache, Path::new("/home/a/.cache/itse"), "a relative XDG path is ignored");
    }

    #[test]
    fn a_missing_or_relative_home_is_refused_unless_xdg_covers_it() {
        let err = |vars: &[(&str, &str)]| {
            let vars: BTreeMap<&str, OsString> = vars.iter().map(|(k, v)| (*k, OsString::from(v))).collect();
            Dirs::from_env(|k| vars.get(k).cloned()).err().map(|e| e.to_string())
        };
        let msg = "HOME is not an absolute path and XDG_CONFIG_HOME is unset; itse keeps its state under them";
        assert_eq!(err(&[]).as_deref(), Some(msg));
        assert_eq!(err(&[("HOME", "relative")]).as_deref(), Some(msg));
        assert_eq!(err(&[("HOME", ""), ("XDG_CONFIG_HOME", "/c"), ("XDG_DATA_HOME", "/d")]).as_deref(), Some(msg.replace("CONFIG", "STATE").as_str()));
        assert_eq!(err(&[("XDG_CONFIG_HOME", "/c"), ("XDG_DATA_HOME", "/d"), ("XDG_STATE_HOME", "/s"), ("XDG_CACHE_HOME", "/k")]), None);
    }

    #[test]
    fn the_socket_path_ignores_xdg_runtime_dir() {
        let tmp = TempDir::new("itse-ws");
        let ws = Workspace::find(None, &tmp.0).unwrap();
        let d = dirs(&[("HOME", "/home/agent"), ("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let sock = ws.socket(&d, &tmp.0).unwrap();
        assert_eq!(sock, PathBuf::from(format!("/home/agent/.local/state/itse/ws/{}/itse.sock", ws.ws)));
        assert_eq!(sock.as_os_str().len(), 55);
    }

    #[test]
    fn a_long_state_path_puts_the_socket_in_tmp() {
        let tmp = TempDir::new("itse-ws");
        let ws = Workspace::find(None, &tmp.0).unwrap();
        let d = dirs(&[("HOME", &format!("/home/{}", "x".repeat(60)))]);
        let sock = ws.socket(&d, &tmp.0).unwrap();
        assert_eq!(sock, tmp.0.join(format!("itse-{}/{}.sock", sys::uid(), ws.ws)));
        assert_eq!(fs::metadata(sock.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_tmp_socket_path_that_still_overflows_is_refused() {
        let tmp = TempDir::new("itse-ws");
        let ws = Workspace::find(None, &tmp.0).unwrap();
        let long_tmp = tmp.0.join("y".repeat(100));
        let err = ws.socket(&dirs(&[("HOME", &format!("/{}", "x".repeat(80)))]), &long_tmp).unwrap_err().to_string();
        assert!(err.starts_with("socket path ") && err.ends_with(" is longer than 107 bytes"), "{err}");
        assert!(!long_tmp.exists(), "nothing is created for a socket that can't be bound");
    }

    #[test]
    fn a_non_private_tmp_dir_is_refused() {
        let tmp = TempDir::new("itse-ws");
        let ws = Workspace::find(None, &tmp.0).unwrap();
        let shared = tmp.0.join(format!("itse-{}", sys::uid()));
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let err = ws.socket(&dirs(&[("HOME", &format!("/{}", "x".repeat(80)))]), &tmp.0).unwrap_err().to_string();
        assert_eq!(err, format!("socket dir {} is not private (owner {}, mode 777); remove it", shared.display(), sys::uid()));
    }
}
