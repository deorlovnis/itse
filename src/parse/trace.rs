//! Trace trimming: a failure's stack trace cut to what an agent reads first. Line 1
//! and every `Caused by:` stay, with up to 3 message lines; the first frame of each cause stays, as
//! does every frame outside the framework (`frames.collapse`), located in the repo; framework frames
//! become one `… N framework frames` line; `classify` adds a `class:` line.
//!
//! A frame names a file, not a path, so it is found by suffix (`com/acme/Order.kt`) in a file index
//! built once per parse: `git ls-files` (with no hooks, fsmonitor or inherited `GIT_*`), else a walk that skips build output; at most 50K names.

use std::path::Path;
use std::process::{Command, Stdio};

use regex_lite::Regex;

use crate::manifest::{Classify, Frames};
use crate::results::Loc;

/// The repo's file names, built on first use.
pub struct Index<'a> {
    root: &'a Path,
    names: Option<(Vec<String>, bool)>,
}

pub const INDEX_CAP: usize = 50_000;

impl<'a> Index<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root, names: None }
    }

    /// The repo path ending in `dir/file`, else the first one named `file`; and whether the index
    /// was cut at `INDEX_CAP`.
    pub fn find(&mut self, dir: &str, file: &str) -> (Option<String>, bool) {
        let root = self.root;
        let (names, cut) = self.names.get_or_insert_with(|| {
            // The repo's config could name commands (core.fsmonitor) and inherited GIT_* vars could
            // point at another repo: neither applies, as the listing runs no project code.
            let mut git = Command::new("git");
            for (k, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("GIT_")) {
                git.env_remove(k);
            }
            git.envs([("GIT_CONFIG_NOSYSTEM", "1"), ("GIT_CONFIG_GLOBAL", "/dev/null")]).args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C"]);
            let git = git.arg(root).args(["ls-files", "-z"]).stdin(Stdio::null()).stderr(Stdio::null()).output();
            let listed = git.ok().filter(|o| o.status.success() && !o.stdout.is_empty());
            let names = listed.map(|o| String::from_utf8_lossy(&o.stdout).split('\0').filter(|n| !n.is_empty()).take(INDEX_CAP + 1).map(String::from).collect::<Vec<_>>());
            let (mut names, _) = names.map_or_else(|| walk(root, &[".git", "build", "target", "node_modules"], INDEX_CAP + 1), |n| (n, false));
            let cut = names.len() > INDEX_CAP;
            names.truncate(INDEX_CAP);
            (names, cut)
        });
        let full = format!("{dir}/{file}");
        let named = |n: &&String| n.rsplit('/').next() == Some(file);
        (names.iter().find(|n| n.ends_with(&full) && (n.len() == full.len() || n.as_bytes()[n.len() - full.len() - 1] == b'/')).or(names.iter().find(named)).cloned(), *cut)
    }
}

/// Repo-relative paths of the files under `root`, skipping directories named in `skip`; at most
/// `cap`, and whether it stopped there.
pub fn walk(root: &Path, skip: &[&str], cap: usize) -> (Vec<String>, bool) {
    let (mut out, mut todo) = (Vec::new(), vec![root.to_path_buf()]);
    while let Some(dir) = todo.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (path, kind) = (e.path(), e.file_type());
            if kind.as_ref().is_ok_and(|k| k.is_dir()) && !skip.iter().any(|s| e.file_name() == **s) {
                todo.push(path);
            } else if kind.is_ok_and(|k| k.is_file()) {
                out.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
                if out.len() >= cap {
                    return (out, true);
                }
            }
        }
    }
    (out, false)
}

/// Trim a failure's text to detail lines. A located frame of `class` (the test's), else the first
/// located frame, is where the failure is. Returns the lines, that location, and whether the file
/// index was cut.
pub fn trim(text: &str, class: &str, frames: Option<&Frames>, classify: &[Classify], index: &mut Index) -> (Vec<String>, Option<Loc>, bool) {
    let re = frames.and_then(|f| Some((f, Regex::new(&f.regexp).ok()?, f.collapse.iter().filter_map(|c| Regex::new(c).ok()).collect::<Vec<_>>())));
    let (mut out, mut loc, mut first, mut collapsed, mut message, mut cut, mut in_class) = (Vec::new(), None, true, 0, 0, false, false);
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let frame = re.as_ref().and_then(|(f, re, collapse)| Some((f, re.captures(line)?, collapse)));
        let Some((f, c, collapse)) = frame else {
            if i == 0 || line.trim_start().starts_with("Caused by:") {
                (first, message) = (true, 0);
                out.push(line.trim().to_string());
            } else if first && message < 2 && !line.trim_start().starts_with("...") {
                message += 1;
                out.push(line.trim().to_string());
            }
            continue;
        };
        let at = line.trim_start().trim_start_matches("at ");
        if !first && collapse.iter().any(|c| c.is_match(at)) {
            collapsed += 1;
            continue;
        }
        let call = at.split('(').next().unwrap_or(at);
        let dir = call.rsplitn(3, '.').nth(2).unwrap_or("").replace('.', "/");
        let (file, n) = (c.get(f.file).map_or("", |m| m.as_str()), c.get(f.line).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(1));
        let (path, truncated) = index.find(&dir, file);
        cut |= truncated;
        out.push(format!("at {call}  {}:{n}", path.as_deref().unwrap_or(file)));
        out.extend(path.as_deref().and_then(|p| super::source(index.root, p, n)).map(|(n, l)| format!("  {n:>5} | {}", l.trim_end())));
        let mine = !class.is_empty() && call.starts_with(class);
        if path.is_some() && (loc.is_none() || (mine && !in_class)) {
            (loc, in_class) = (path.map(|path| Loc { path, line: n, col: 1 }), mine);
        }
        first = false;
    }
    out.extend((collapsed > 0).then(|| format!("… {collapsed} framework frames")));
    out.extend(
        classify
            .iter()
            .filter(|c| Regex::new(&c.regexp).is_ok_and(|re| re.is_match(text)))
            .map(|c| format!("class: {}{}", c.class, c.note.as_ref().map_or(String::new(), |n| format!(" ({n})")))),
    );
    (out, loc, cut)
}
