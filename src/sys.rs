//! The few things Itse needs from the OS beyond std: libc calls declared by hand, process state
//! from /proc, private folders and files, random bytes, and UTC timestamps.
//!
//! No libc crate (as Halti): six calls with Linux constants, each behind a small safe function.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

// --- libc, declared by hand (Linux values, the same on aarch64 and x86_64) ---

pub const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;
const SIGHUP: i32 = 1;
pub const RLIMIT_CPU: i32 = 0;
pub const RLIMIT_CORE: i32 = 4;
pub const RLIMIT_NOFILE: i32 = 7;
pub const RLIMIT_AS: i32 = 9;
const M_ARENA_MAX: i32 = -8;
/// An `open` flag: a FIFO opens at once instead of waiting for a writer.
pub const O_NONBLOCK: i32 = 0o4000;

mod ffi {
    unsafe extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
        pub fn setsid() -> i32;
        pub fn setrlimit(resource: i32, limit: *const [u64; 2]) -> i32;
        pub fn getuid() -> u32;
        pub fn signal(sig: i32, handler: usize) -> usize;
        pub fn mallopt(param: i32, value: i32) -> i32;
    }
}

/// Send `sig` to a process, or to a process group when `pid` is negative. False if none got it.
pub fn send_signal(pid: i32, sig: i32) -> bool {
    // SAFETY: kill takes plain integers and touches no memory of ours.
    unsafe { ffi::kill(pid, sig) == 0 }
}

/// Start a new session, so a harness killing its tool's process tree misses the child. For `pre_exec`.
pub fn new_session() -> io::Result<()> {
    // SAFETY: setsid takes no arguments and is async-signal-safe, so it may run between fork and exec.
    if unsafe { ffi::setsid() } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Set both the soft and the hard limit. For `pre_exec`: it allocates nothing.
pub fn set_rlimit(resource: i32, value: u64) -> io::Result<()> {
    let limit = [value, value];
    // SAFETY: the pointer is to a live [u64; 2], the layout of struct rlimit on 64-bit Linux.
    if unsafe { ffi::setrlimit(resource, &limit) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

pub fn uid() -> u32 {
    // SAFETY: getuid can't fail and touches no memory of ours.
    unsafe { ffi::getuid() }
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// Whether SIGHUP, SIGINT or SIGTERM arrived since `catch_signals` or the last call, which clears
/// the flag, so the CLI can tell a second Ctrl-C from the first.
pub fn signalled() -> bool {
    SIGNALLED.swap(false, Ordering::SeqCst)
}

extern "C" fn on_signal(_signal: i32) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// Have the signals that end a session set a flag instead (Halti run.rs). glibc's `signal` sets
/// SA_RESTART, so a blocked call isn't interrupted: whoever waits must poll `signalled()`.
pub fn catch_signals() {
    for sig in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { ffi::signal(sig, on_signal as extern "C" fn(i32) as usize) };
    }
}

/// Two malloc arenas at most, so each allocating thread of the daemon doesn't get its own.
pub fn limit_arenas() {
    // SAFETY: mallopt takes plain integers; M_ARENA_MAX only caps arenas made later.
    unsafe { ffi::mallopt(M_ARENA_MAX, 2) };
}

// --- processes ---

/// What /proc says about a process: its state letter, its group and its start time in ticks since boot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcStat {
    pub state: char,
    pub pgid: i32,
    pub start_time: u64,
}

impl ProcStat {
    /// A zombie (Z) or dead task (X) has exited; only its entry is left.
    pub fn alive(&self) -> bool {
        !matches!(self.state, 'Z' | 'X')
    }
}

pub fn proc_stat(pid: i32) -> Option<ProcStat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Whether `pid` still runs and is the process we started: a reused pid has another start time.
pub fn alive(pid: i32, start_time: u64) -> bool {
    proc_stat(pid).is_some_and(|s| s.alive() && s.start_time == start_time)
}

/// The fields after the command name, which is in parentheses and may itself hold ") ".
fn parse_stat(text: &str) -> Option<ProcStat> {
    let mut fields = text[text.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    // Fields 5 (the group, after the parent) and 22, read in this order.
    Some(ProcStat { state, pgid: fields.nth(1)?.parse().ok()?, start_time: fields.nth(16)?.parse().ok()? })
}

/// Poll `done` every 20 ms for up to `limit`; whether it came true.
pub fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let (end, mut ok) = (Instant::now() + limit, done());
    while !ok && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
        ok = done();
    }
    ok
}

/// SIGTERM to the group, then SIGKILL to whatever is left after `grace`. `reap` is a non-blocking
/// wait on the leader when it is our child: its zombie still counts as a member until reaped.
pub fn kill_group(pgid: i32, grace: Duration, mut reap: impl FnMut()) {
    // -1 would signal every process of ours, and 0 our own group.
    if pgid <= 1 {
        return;
    }
    send_signal(-pgid, SIGTERM);
    let gone = wait_until(grace, || {
        reap();
        !send_signal(-pgid, 0)
    });
    if !gone {
        send_signal(-pgid, SIGKILL);
    }
}

/// Lock a mutex even if a panicking thread poisoned it, so one panic can't disable a server for
/// the daemon's lifetime.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `git -C root` for a read-only listing that runs no project code. The repo's config can name
/// commands: core.fsmonitor and hooks are turned off, and so is every filter driver it (or a file
/// it includes) declares, since git runs a file's clean or process filter whenever it rehashes a
/// file whose stat changed; each `filter.*` key is blanked through `GIT_CONFIG_KEY_n`, which takes
/// any subsection name, and a blank `required` is false. Listing them reads config only. No
/// transport may run (`GIT_ALLOW_PROTOCOL` empty overrides the repo's `protocol.*.allow`), so a
/// partial clone fetches nothing. `GIT_*` vars inherited from a git hook could point at another
/// repo or index, so none applies. Optional locks are off: it never takes `.git/index.lock` or
/// rewrites the index.
pub fn git(root: &Path) -> process::Command {
    let base = || {
        let mut git = process::Command::new("git");
        for (k, _) in env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
            git.env_remove(k);
        }
        git.envs([("GIT_CONFIG_NOSYSTEM", "1"), ("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_OPTIONAL_LOCKS", "0"), ("GIT_ALLOW_PROTOCOL", "")]);
        git.args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C"]).arg(root);
        git.stdin(process::Stdio::null()).stderr(process::Stdio::null());
        git
    };
    let filters = base().args(["config", "-z", "--name-only", "--get-regexp", r"^filter\."]).output().map_or(vec![], |o| o.stdout);
    let mut git = base();
    let keys: Vec<&[u8]> = filters.split(|&b| b == 0).filter(|k| !k.is_empty()).collect();
    git.env("GIT_CONFIG_COUNT", keys.len().to_string());
    for (i, key) in keys.iter().enumerate() {
        git.env(format!("GIT_CONFIG_KEY_{i}"), std::ffi::OsStr::from_bytes(key)).env(format!("GIT_CONFIG_VALUE_{i}"), "");
    }
    git
}

// --- private files ---

/// A folder only we can enter: made 0700 with its parents, a looser one of ours tightened, a link
/// or another user's folder refused.
pub fn private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid() {
        bail!("{} is not a directory of ours", dir.display());
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a 0600 file next to `path` and rename it over `path`, so readers never see half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    let written = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(bytes)).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

fn temp_path(path: &Path) -> PathBuf {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{}", process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
    path.with_file_name(name)
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).context("reading /dev/urandom")?;
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The per-machine salt of the usage log's ids: 32 random bytes in `<state>/salt`, made once.
/// A hard link publishes it, so two first writers agree on one salt and none reads half a file.
pub fn salt(state: &Path) -> Result<[u8; 32]> {
    let path = state.join("salt");
    let read = |path: &Path| fs::read(path).ok().and_then(|b| b.try_into().ok());
    if let Some(salt) = read(&path) {
        return Ok(salt);
    }
    private_dir(state)?;
    let tmp = temp_path(&path);
    write_atomic(&tmp, &random_bytes::<32>()?)?;
    let _ = fs::hard_link(&tmp, &path);
    let _ = fs::remove_file(&tmp);
    read(&path).ok_or_else(|| anyhow!("{} is not 32 bytes; remove it", path.display()))
}

// --- time and test seams ---

/// RFC 3339 UTC with milliseconds, `2026-11-04T18:31:07.412Z` (civil-from-days, no time crate).
pub fn utc(t: SystemTime) -> String {
    let ms = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

/// Release builds refuse the debug-only `ITSE_TEST_*` seams, so a stray one can't change what a
/// real install does.
pub fn refuse_test_seams() -> Result<()> {
    if cfg!(debug_assertions) {
        return Ok(());
    }
    match env::vars_os().find(|(k, _)| k.as_encoded_bytes().starts_with(b"ITSE_TEST_")) {
        Some((k, _)) => bail!("{} is a test setting; release builds refuse it", k.display()),
        None => Ok(()),
    }
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, UNIX_EPOCH};
    use std::{env, fs, process, thread};

    use super::*;

    /// A private temporary directory (mode 700), removed when dropped (Halti util.rs).
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(prefix: &str) -> Self {
            static COUNT: AtomicUsize = AtomicUsize::new(0);
            let dir = env::temp_dir().join(format!("{prefix}-{}-{}", process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
            let _ = fs::remove_dir_all(&dir);
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self(fs::canonicalize(dir).unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // A test store is read-only; u+w comes back first so the folder can go.
            let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&self.0).status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn modes_are_0700_and_0600() {
        let tmp = TempDir::new("itse-sys");
        let state = tmp.0.join("a/b/itse");
        private_dir(&state).unwrap();
        assert_eq!((mode(&tmp.0.join("a")), mode(&state)), (0o700, 0o700));
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&state).unwrap();
        assert_eq!(mode(&state), 0o700, "a looser folder of ours is tightened");
        write_atomic(&state.join("procs.json"), b"[]").unwrap();
        assert_eq!(mode(&state.join("procs.json")), 0o600);
        salt(&state).unwrap();
        assert_eq!(mode(&state.join("salt")), 0o600);
    }

    #[test]
    fn a_link_is_not_a_private_dir() {
        let tmp = TempDir::new("itse-sys");
        std::os::unix::fs::symlink(&tmp.0, tmp.0.join("link")).unwrap();
        assert!(private_dir(&tmp.0.join("link")).unwrap_err().to_string().contains("not a directory of ours"));
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_temp_file() {
        let tmp = TempDir::new("itse-sys");
        let path = tmp.0.join("next.json");
        write_atomic(&path, b"{\"run\":1}").unwrap();
        write_atomic(&path, b"{\"run\":2}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"run\":2}");
        assert_eq!(fs::read_dir(&tmp.0).unwrap().count(), 1);
    }

    #[test]
    fn the_salt_is_made_once_and_kept() {
        let tmp = TempDir::new("itse-sys");
        let first = salt(&tmp.0).unwrap();
        assert_eq!(salt(&tmp.0).unwrap(), first);
        assert_ne!(first, [0; 32]);
        fs::write(tmp.0.join("salt"), b"short").unwrap();
        assert!(salt(&tmp.0).unwrap_err().to_string().contains("is not 32 bytes"));
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_bytes::<8>().unwrap(), random_bytes::<8>().unwrap());
        assert_eq!(hex(&[0, 0xab]), "00ab");
        assert_eq!(sha256_hex(b"").len(), 64);
    }

    #[test]
    fn timestamps_are_rfc_3339_utc_with_milliseconds() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(UNIX_EPOCH + Duration::from_millis(1_793_817_067_412)), "2026-11-04T18:31:07.412Z");
        assert_eq!(utc(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
        assert_eq!(utc(UNIX_EPOCH + Duration::from_secs(4_107_542_399)), "2100-02-28T23:59:59.000Z");
    }

    #[test]
    fn a_stat_line_whose_name_holds_parens_parses() {
        let line = "42 (a) b (c)) S 1 43 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_stat(line), Some(ProcStat { state: 'S', pgid: 43, start_time: 987_654 }));
        assert_eq!(parse_stat("42 (x"), None);
    }

    #[test]
    fn our_own_process_is_alive_and_a_wrong_start_time_is_not() {
        let me = proc_stat(process::id() as i32).unwrap();
        assert!(me.alive());
        assert!(alive(process::id() as i32, me.start_time));
        assert!(!alive(process::id() as i32, me.start_time + 1));
    }

    #[test]
    fn a_zombie_counts_as_dead() {
        let mut child = Command::new("true").spawn().unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while proc_stat(child.id() as i32).is_some_and(|s| s.alive()) && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(proc_stat(child.id() as i32).map(|s| s.state), Some('Z'));
        child.wait().unwrap();
    }

    #[test]
    fn kill_group_ends_a_group_with_sigterm() {
        let mut child = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let (pgid, started) = (child.id() as i32, Instant::now());
        kill_group(pgid, Duration::from_secs(3), || drop(child.try_wait()));
        assert!(started.elapsed() < Duration::from_secs(1), "reaping the leader ends the wait early: {:?}", started.elapsed());
        assert_eq!(child.wait().unwrap().signal(), Some(SIGTERM));
    }

    #[test]
    fn kill_group_escalates_to_sigkill_when_sigterm_is_ignored() {
        let mut child = Command::new("sh").args(["-c", "trap '' TERM; sleep 30 & wait"]).process_group(0).spawn().unwrap();
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        kill_group(child.id() as i32, Duration::from_millis(300), || ());
        assert_eq!(child.wait().unwrap().signal(), Some(SIGKILL));
        assert!(started.elapsed() >= Duration::from_millis(300));
        // SIGKILL is delivered asynchronously, and the orphaned sleep is reaped by init.
        let end = Instant::now() + Duration::from_secs(5);
        while send_signal(-(child.id() as i32), 0) && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!send_signal(-(child.id() as i32), 0), "the background sleep went with its group");
    }

    #[test]
    fn rlimits_apply_in_the_child() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "ulimit -n"]);
        // SAFETY: set_rlimit only calls setrlimit, which is async-signal-safe.
        unsafe { cmd.pre_exec(|| set_rlimit(RLIMIT_NOFILE, 77)) };
        assert_eq!(String::from_utf8(cmd.output().unwrap().stdout).unwrap().trim(), "77");
    }

    #[test]
    fn a_new_session_leads_its_own_group() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo $$ $(cut -d' ' -f6 /proc/$$/stat)"]);
        // SAFETY: new_session only calls setsid, which is async-signal-safe.
        unsafe { cmd.pre_exec(new_session) };
        let out = String::from_utf8(cmd.output().unwrap().stdout).unwrap();
        let (pid, session) = out.trim().split_once(' ').unwrap();
        assert_eq!(pid, session, "the child's session id is its own pid");
    }

    #[test]
    fn a_poisoned_lock_still_locks() {
        let m = std::sync::Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(m.is_poisoned());
        *lock(&m) += 1;
        assert_eq!(*lock(&m), 2);
    }
}
