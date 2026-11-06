//! Parsers of action output and reports, and the problems baselines: the
//! problemMatcher subset over a run's log, JUnit XML reports (`parse/junit.rs`), trace trimming with
//! a bounded file index (`parse/trace.rs`), counts, fingerprints, and merging and ordering items.
//!
//! Every tool specific is extension data; the core knows no compiler or test framework by name. The
//! log is read a line at a time and reports are streamed, so a long run's output never sits in the
//! daemon's memory whole.

mod junit;
mod trace;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use regex_lite::{Captures, Regex};
use serde::{Deserialize, Serialize};

use crate::actions::RunRecord;
use crate::lsp::Pushed;
use crate::manifest::{Action, Extension, Manifest, Matcher, MatcherRef, OneOrMany, Pattern};
use crate::results::{Item, Loc, Outcome, Store, Verdict};
use crate::sys;
use crate::workspace::{Dirs, Workspace};

/// One entry of `baselines.json`, keyed by `<via>:<session key>`: the fingerprints last
/// returned under that key, so `problems --new` gives the delta. Each keeps its kind and path, so a
/// scoped call (or one without `--all`) replaces only the part it showed, then its sources, so a
/// server that wasn't asked keeps its part.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Baseline {
    pub fingerprints: BTreeMap<String, Vec<String>>,
    pub at: String,
}

/// What a run's log and reports hold: items, the test counts `[passed, failed, skipped]` when the
/// action says where they are, and notes.
#[derive(Debug, Default)]
pub struct Parsed {
    pub items: Vec<Item>,
    pub counts: Option<[u64; 3]>,
    pub notes: Vec<String>,
}

fn regex(re: &str, of: &str) -> Result<Regex> {
    Regex::new(re).map_err(|e| anyhow!("{of}: regex '{re}' is refused (regex-lite has no lookaround or backreferences): {e}"))
}

/// Compile every regex an extension declares, so one regex-lite can't run (lookaround,
/// backreferences) fails when the extension loads, not after its action has run.
pub fn check(ext: &Extension) -> Result<()> {
    let inline = ext.actions.iter().flat_map(|a| a.problem_matcher.iter().flat_map(OneOrMany::as_slice));
    let inline = inline.filter_map(|r| if let MatcherRef::Inline(m) = r { Some(&**m) } else { None });
    for p in ext.problem_matchers.values().chain(inline).flat_map(|m| m.pattern.iter().flat_map(OneOrMany::as_slice)) {
        regex(&p.regexp, "problemMatcher")?;
    }
    for a in &ext.actions {
        let frames = a.frames.iter().flat_map(|f| [&f.regexp].into_iter().chain(&f.collapse));
        for re in frames.chain(a.classify.iter().map(|c| &c.regexp)).chain(a.counts.iter().map(|c| &c.regexp)).chain(&a.drop_lines).chain(&a.ready) {
            regex(re, &a.label)?;
        }
    }
    // A pattern that can't compile would hide nothing.
    ext.servers.iter().try_for_each(|s| s.hide_vars.iter().try_for_each(|h| regex(h, &format!("{} hide_vars", s.id)).map(drop)))
}

/// A matcher in use: its compiled patterns, and the item it is part way through (the next pattern
/// to match, the item, and how many detail lines it may hold).
struct Live<'a> {
    m: &'a Matcher,
    tool: String,
    patterns: Vec<(Regex, &'a Pattern)>,
    open: Option<(usize, Item, usize)>,
}

impl Live<'_> {
    /// Feed one line: a fixed multi-line pattern matches line by line, then up to `detail_lines`
    /// lines until a blank one; a line that breaks a pattern ends it and may start the next.
    fn feed(&mut self, root: &Path, line: &str, items: &mut Vec<Item>) {
        let (len, extra) = (self.patterns.len(), self.m.detail_lines.unwrap_or(0));
        if let Some((i, item, limit)) = self.open.as_mut() {
            if *i == len && !line.trim().is_empty() && item.detail.len() < *limit {
                return item.detail.push(line.to_string());
            } else if *i == len {
                items.extend(self.open.take().map(|o| o.1));
            } else if !self.patterns[*i].0.is_match(line) {
                self.open = None;
            }
        }
        if self.open.is_none() && self.patterns.first().is_some_and(|p| p.0.is_match(line)) {
            // A test item is the test's own; only a problem names the tool that reported it.
            let test = self.m.kind.as_deref() == Some("test");
            let sources = vec![self.tool.clone()].into_iter().filter(|_| !test).collect();
            self.open = Some((0, Item { kind: (if test { "failed" } else { "error" }).into(), sources, ..Item::default() }, 0));
        }
        let Some((i, item, limit)) = self.open.as_mut() else { return };
        let Some(c) = self.patterns[*i].0.captures(line) else { return };
        fill(root, self.m, self.patterns[*i].1, &c, item);
        (*i, *limit) = (*i + 1, item.detail.len() + extra);
        // A pattern without detail lines is whole at its last line.
        if *i == len && extra == 0 {
            items.extend(self.open.take().map(|o| o.1));
        }
    }
}

/// Set an item's fields from one pattern's captures. A test's message goes to its detail, under
/// the test's name.
fn fill(root: &Path, m: &Matcher, p: &Pattern, c: &Captures, item: &mut Item) {
    let g = |n: Option<usize>| n.and_then(|n| c.get(n)).map(|m| m.as_str().to_string());
    let num = |n| g(n).and_then(|s| s.parse().ok()).unwrap_or(1);
    if let Some(file) = g(p.file) {
        let base = match m.file_location.as_ref().map(OneOrMany::as_slice) {
            Some([_, dir]) => root.join(dir),
            _ => root.to_path_buf(),
        };
        let path = base.join(file.trim_start_matches("file://"));
        let path = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        item.loc = Some(Loc { path, line: num(p.line), col: num(p.column) });
    }
    if let Some(s) = g(p.severity).or_else(|| m.severity.clone()).filter(|_| item.kind != "failed") {
        item.kind = [("e", "error"), ("w", "warning"), ("i", "info"), ("h", "hint")].iter().find(|(k, _)| s.to_lowercase().starts_with(k)).map_or("error", |k| k.1).into();
    }
    let message = g(p.message).map(|msg| g(p.code).map_or(msg.clone(), |code| format!("{code}: {msg}")));
    item.msg = g(p.name).unwrap_or(std::mem::take(&mut item.msg));
    match message {
        Some(m) if item.kind == "failed" => item.detail.push(m),
        Some(m) => item.msg = m,
        None => {}
    }
}

/// Parse a finished run of `a`: its matchers (named ones looked up in `named`), `drop_lines` and
/// `counts` over the log, then its reports (`junit-xml:<glob>`, fresh against `started`). The items
/// get their source lines and are merged.
pub fn run(root: &Path, a: &Action, named: &BTreeMap<&str, &Matcher>, log: &Path, started: SystemTime) -> Result<Parsed> {
    let (mut out, mut live) = (Parsed { notes: a.notes.clone(), ..Parsed::default() }, Vec::new());
    for r in a.problem_matcher.iter().flat_map(OneOrMany::as_slice) {
        let (tool, m) = match r {
            MatcherRef::Name(n) => (n.trim_start_matches('$'), *named.get(n.trim_start_matches('$')).ok_or_else(|| anyhow!("no problemMatcher '{n}' in the extensions"))?),
            MatcherRef::Inline(m) => (a.label.as_str(), &**m),
        };
        let patterns = m.pattern.iter().flat_map(OneOrMany::as_slice).map(|p| Ok((regex(&p.regexp, tool)?, p))).collect::<Result<_>>()?;
        live.push(Live { m, tool: m.owner.clone().or(m.source.clone()).unwrap_or(tool.into()), patterns, open: None });
    }
    let drop = a.drop_lines.iter().map(|d| regex(d, &a.label)).collect::<Result<Vec<_>>>()?;
    let counts = a.counts.as_ref().map(|c| regex(&c.regexp, &a.label).map(|re| (re, c))).transpose()?;
    for line in BufReader::new(File::open(log)?).split(b'\n') {
        let line = String::from_utf8_lossy(&line?).trim_end_matches('\r').to_string();
        if drop.iter().any(|d| d.is_match(&line)) {
            continue;
        }
        if let Some((c, spec)) = counts.as_ref().and_then(|(re, spec)| Some((re.captures(&line)?, spec))) {
            let n = |g: Option<usize>| g.and_then(|g| c.get(g)?.as_str().parse::<u64>().ok()).unwrap_or(0);
            let sum = out.counts.get_or_insert([0; 3]);
            (sum[0], sum[1], sum[2]) = (sum[0] + n(spec.passed), sum[1] + n(spec.failed), sum[2] + n(spec.skipped));
        }
        live.iter_mut().for_each(|l| l.feed(root, &line, &mut out.items));
    }
    out.items.extend(live.iter_mut().filter_map(|l| l.open.take().filter(|o| o.0 == l.patterns.len()).map(|o| o.1)));
    for it in &mut out.items {
        it.src.extend(it.loc.as_ref().and_then(|l| source(root, &l.path, l.line)));
    }
    let mut index = trace::Index::new(root);
    for glob in a.parse.iter().filter_map(|p| p.strip_prefix("junit-xml:")) {
        junit::reports(root, glob, started, a, &mut index, &mut out)?;
    }
    out.items = merge(std::mem::take(&mut out.items));
    Ok(out)
}

/// Parse an ended run onto its outcome, with its action and the matchers as the manifest declares
/// them now (the runner keeps no copy). A run the manifest no longer declares stays as it ended; a
/// parse that fails is a note, never a lost result.
pub fn ended(ws: &Workspace, dirs: &Dirs, rec: &RunRecord, dir: &Path, o: &mut Outcome) {
    let parsed = Manifest::load(ws, dirs).and_then(|m| {
        let uses = m.uses();
        let Some(a) = uses.iter().filter_map(|u| m.exts.get(&u.id)).flat_map(|e| &e.0.actions).find(|a| a.label == rec.spec.label) else { return Ok(None) };
        let named = m.exts.values().flat_map(|e| e.0.problem_matchers.iter().map(|(k, m)| (k.as_str(), m))).collect();
        run(&ws.root, a, &named, &dir.join("out.log"), UNIX_EPOCH + Duration::from_millis(rec.spec.started_ms)).map(Some)
    });
    if let Some(p) = parsed.unwrap_or_else(|e| Some(Parsed { notes: vec![format!("not parsed: {e:#}")], ..Parsed::default() })) {
        p.apply(o);
    }
}

/// Line `n` of a repo file, as an item's source line.
pub fn source(root: &Path, path: &str, n: u32) -> Option<(u32, String)> {
    BufReader::new(File::open(root.join(path)).ok()?).lines().nth(n.checked_sub(1)? as usize)?.ok().map(|l| (n, l))
}

impl Parsed {
    /// Put the items, counts and notes on a run's outcome: they replace `exit N` and the log's tail
    /// (the `log:` line stays); a test run that reports failures fails.
    pub fn apply(self, o: &mut Outcome) {
        let n = |kind: &str| self.items.iter().filter(|i| i.kind == kind).count();
        let tests = self.counts.map(|[p, f, s]| format!("{f} failed, {p} passed, {s} skipped"));
        let problems = ["error", "warning"].map(|k| (n(k) > 0).then(|| plural(n(k), k)));
        let parts: Vec<String> = [tests].into_iter().chain(problems).flatten().collect();
        if !parts.is_empty() {
            o.counts = if o.counts.starts_with("exit") { parts.join(", ") } else { format!("{}; {}", o.counts, parts.join(", ")) };
        }
        if !self.items.is_empty() {
            o.notes.retain(|l| l.starts_with("log: "));
        }
        if o.verdict == Verdict::Ok && (self.counts.is_some_and(|c| c[1] > 0) || n("error") + n("failed") > 0) {
            o.verdict = Verdict::Fail;
        }
        o.notes.splice(0..0, self.notes);
        o.items = self.items;
    }
}

pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// sha256 of (path, message head with digits as `#`, trimmed source line): it holds when lines
/// move. The tool is left out, so one problem reported by two tools merges into one item; so is
/// the detail, which a server adds and a compiler's one-line form lacks, except a failed test's
/// message; and so is what follows the message's first `: ` after a space (after a code such as
/// `E0277: `) unless a quoted name follows, since rustc's short form appends the span's label
/// there, and rust-analyzer doesn't.
pub fn fingerprint(it: &Item) -> String {
    let detail = it.detail.first().filter(|_| it.kind == "failed");
    let msg: String = head(&it.msg).chars().chain(detail.into_iter().flat_map(|d| d.chars())).map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
    let (path, line) = (it.loc.as_ref().map_or("", |l| l.path.as_str()), it.src.first().map_or("", |s| s.1.trim()));
    sys::sha256_hex([it.kind.as_str(), path, &msg, line].join("\0").as_bytes())
}

/// A message up to its first `: ` that follows a space and doesn't lead to a quoted name: `E0277:
/// cannot add`, not its label, but all of `unused variable: `x``, which another name makes another problem.
fn head(msg: &str) -> &str {
    let cut = msg.match_indices(": ").map(|(i, _)| i).find(|&i| msg[..i].contains(' ') && !msg[i + 2..].starts_with('`'));
    &msg[..cut.unwrap_or(msg.len())]
}

/// Merge duplicates (one fingerprint at one place, as when a lib and its tests both report it, or
/// a compiler and a language server; their sources joined, the longer message kept), order by
/// severity, path and line, and number the ids per kind: E1, W1,
/// I1, H1, and T1 for failed tests. Alike problems on different lines stay apart.
pub fn merge(items: Vec<Item>) -> Vec<Item> {
    let mut out: Vec<(String, Item)> = Vec::new();
    let at = |it: &Item| it.loc.as_ref().map(|l| (l.path.clone(), l.line, l.col));
    for it in items {
        let fp = fingerprint(&it);
        match out.iter_mut().find(|(f, kept)| *f == fp && at(kept) == at(&it)) {
            Some((_, kept)) => {
                if it.msg.len() > kept.msg.len() {
                    kept.msg = it.msg;
                    let msg = &kept.msg;
                    kept.detail.retain(|d| !msg.ends_with(d.as_str()));
                }
                kept.sources.extend(it.sources.into_iter().filter(|s| !kept.sources.contains(s)).collect::<BTreeSet<_>>())
            }
            None => out.push((fp, it)),
        }
    }
    const KINDS: [(&str, &str); 5] = [("error", "E"), ("failed", "T"), ("warning", "W"), ("info", "I"), ("hint", "H")];
    let rank = |k: &str| KINDS.iter().position(|(kind, _)| *kind == k).unwrap_or(0);
    out.sort_by_key(|(_, it)| (rank(&it.kind), it.loc.as_ref().map(|l| (l.path.clone(), l.line, l.col))));
    let mut seen = [0; KINDS.len()];
    for (_, it) in &mut out {
        let r = rank(&it.kind);
        seen[r] += 1;
        it.id = format!("{}{}", KINDS[r].1, seen[r]);
    }
    out.into_iter().map(|(_, it)| it).collect()
}

/// `problems [scope] [--new] [--all]` over the newest result of each action and the diagnostics the
/// language servers `pushed`: merged, errors first, hints only with `--all`, in `scope` (a
/// repo-relative prefix). `--new` shows only what the caller's baseline key `via:session` wasn't
/// given by its last `--new`, counts the unchanged and fixed, and replaces the part of that key's
/// baseline it shows in `baselines.json`; a plain call leaves the baseline as it is, and so does one
/// while a run is still being parsed or a server hasn't `settled`, so what it missed is reported by
/// the next call. The baseline's problems from a `held` server, which wasn't asked, stay in it and
/// are not counted fixed.
pub fn problems(store: &Store, scope: &str, all: bool, new: bool, key: &str, pushed: &mut Pushed) -> Result<Outcome> {
    let (mut settled, held) = (pushed.settled, &pushed.held);
    let (mut labels, mut items, mut notes) = (BTreeSet::new(), std::mem::take(&mut pushed.items), Vec::new());
    for n in store.runs() {
        let dir = store.0.join(format!("runs/{n}"));
        // run.json is small; a result is read only for the newest ended run of each label.
        let Some(rec) = RunRecord::load(&dir).filter(|r| r.exit.is_some() && !labels.contains(&r.spec.label)) else { continue };
        if !dir.join("result.json").exists() {
            // Ended but not yet parsed: the label's older result stands in, and nothing is settled.
            settled = false;
            notes.push(format!("run:{n} of {} is still being parsed", rec.spec.label));
            continue;
        }
        let Ok((o, _)) = store.load(&format!("run:{n}")) else { continue };
        notes.push(format!("from {} run:{n}, ended {}", rec.spec.label, rec.exit.map(|e| e.ended_at).unwrap_or_default()));
        labels.insert(rec.spec.label);
        items.extend(o.items.into_iter().filter(|i| i.kind != "failed"));
    }
    let scope = scope.trim_start_matches("./");
    let kinds: &[&str] = if all { &["error", "warning", "info", "hint"] } else { &["error", "warning", "info"] };
    let shown = |kind: &str, path: &str| kinds.contains(&kind) && (path.is_empty() || path.starts_with(scope));
    let path = |i: &Item| i.loc.as_ref().map_or(String::new(), |l| l.path.clone());
    let view: Vec<(String, Item)> = merge(items).into_iter().filter(|i| shown(&i.kind, &path(i))).map(|i| (fingerprint(&i), i)).collect();
    let n = |items: &[Item], kind: &str| items.iter().filter(|i| i.kind == kind).count();
    let counts = |items: &[Item], new: &str| format!("{}, {}", plural(n(items, "error"), &format!("{new}error")), plural(n(items, "warning"), &format!("{new}warning")));
    let (tail, items) = match new {
        false => (String::new(), view.into_iter().map(|(_, i)| i).collect::<Vec<_>>()),
        true => {
            let shown = |e: &[String]| shown(e.first().map_or("", String::as_str), e.get(1).map_or("", String::as_str));
            let held = |e: &[String]| e.iter().skip(2).any(|s| held.contains(s));
            let next = |b: &Baseline| {
                let kept = b.fingerprints.iter().filter(|(_, e)| !shown(e) || held(e)).map(|(f, e)| (f.clone(), e.clone()));
                let entry = |i: &Item| [i.kind.clone(), path(i)].into_iter().chain(i.sources.iter().cloned()).collect();
                Baseline { fingerprints: kept.chain(view.iter().map(|(f, i)| (f.clone(), entry(i)))).collect(), at: sys::utc(SystemTime::now()) }
            };
            let before = baseline(&store.0, key, settled.then_some(&next))?;
            let fresh: Vec<Item> = view.iter().filter(|(f, _)| !before.fingerprints.contains_key(f)).map(|(_, i)| i.clone()).collect();
            let fixed = before.fingerprints.iter().filter(|(f, e)| shown(e) && !held(e) && !view.iter().any(|(v, _)| v == *f)).count();
            let since = before.at.get(11..19).map_or(String::new(), |t| format!(" since {t}"));
            notes.extend((!settled).then(|| "not settled: the baseline is not advanced".to_string()));
            (format!("; {} unchanged, {fixed} fixed{since}", view.len() - fresh.len()), fresh)
        }
    };
    let counts = counts(&items, if new { "new " } else { "" }) + &tail;
    let verdict = if n(&items, "error") > 0 {
        Verdict::Fail
    } else if items.is_empty() {
        Verdict::Empty
    } else {
        Verdict::Ok
    };
    Ok(Outcome { items, notes, settled: Some(settled), ..Outcome::new("problems", verdict, scope, counts) })
}

/// The baseline of `key`, replaced by `next(baseline)` when given, under a lock so parallel calls
/// don't drop each other's keys.
fn baseline(state: &Path, key: &str, next: Option<&dyn Fn(&Baseline) -> Baseline>) -> Result<Baseline> {
    let (path, _held) = (state.join("baselines.json"), File::create(state.join("baselines.lock")).and_then(|f| f.lock().map(|()| f))?);
    let mut baselines: BTreeMap<String, Baseline> = fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let before = baselines.remove(key).unwrap_or_default();
    if let Some(next) = next {
        baselines.insert(key.into(), next(&before));
        sys::write_atomic(&path, &serde_json::to_vec(&baselines)?)?;
    }
    Ok(before)
}

#[cfg(test)]
mod tests;
