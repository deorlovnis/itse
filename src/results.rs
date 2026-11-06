//! One result schema for every verb: the `Outcome`, its text with caps and pages, the
//! `TEXT` table, the results store and `show`.
//!
//! `--json` prints `{"itse":1, …Outcome}`; the text form is rendered from the same value, so the
//! CLI, the hook and MCP agree on what a result holds. The daemon renders, so the CLI and MCP print
//! the same bytes; only a `next` command is rendered for its caller, as a CLI line or a tool call.
//! Every pointer is fully qualified (`itse show q:7 --page 2`), so it can be followed from anywhere.

use std::fmt::Display;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sys;
use crate::verbs::{Answer, Args};

/// Every core template; `{name}` fields are filled by `text`. Server and compiler messages pass
/// through verbatim and never come from here.
pub const TEXT: &[(&str, &str)] = &[
    ("more", "page {n} of {of}; full: itse show {id} --page {next}"),
    ("last", "page {n} of {of}"),
    ("hidden", "   … {n} more lines: itse show {id}/{item}"),
    ("unknown_id", "no stored result {id}"),
    ("no_item", "no stored result holds {id}"),
    ("picked", "{item} from {id}, the newest result holding it"),
    ("grep", "{n} of {of} lines match"),
    ("tail", "last {n} of {of} lines"),
    ("capped", "… {n} more lines: itse show {id} --raw"),
    ("terminal", "{cmd} in a terminal"),
    ("human", "a human runs this; next (human): {cmd}"),
    ("cli_only", "not served over MCP; it runs from the CLI"),
    ("wrong_tool", "not a verb of {tool}"),
    ("not_built", "not built yet"),
    ("cancelled", "cancelled"),
    ("restarted", "itse daemon restarted; the call did not finish; call again"),
    (
        "instructions",
        "itse runs this workspace's declared tests, builds and language servers and returns located, paged results with ids.\ntest_build_run: test, build, run <label>, install, lock, use, stop, cancel. problems_symbols: problems (compiler and LSP diagnostics), find, refs, info, outline (symbols by name), status, ext. show_result: run:14/T1, pages, grep, tail.",
    ),
    // SKILL.md for a harness: the same in every workspace; `itse verbs` has what differs.
    (
        "skill",
        "---
name: {name}
description: {description}
---
# itse

itse runs this workspace's declared tests, builds and language servers and answers with located, paged results that have ids. Each verb is a shell command, `itse <verb>`, and a call of the itse MCP tools test_build_run, problems_symbols and show_result, whose `args` take the same tokens.

## Verbs

{verbs}
## Results

- The first line is the verdict: `VERDICT verb scope — counts; cost; id`, such as `FAIL test :data — 2 failed, 141 passed; 38.2 s; run:14`.
- Items have ids: E1 (errors), W1 (warnings), T1 (tests), S1 (symbols), R1 (references). `itse show run:14/T1` shows one item in full, `itse show q:7 --page 2` the next page, and `--grep RE` or `--tail N` a part of a log.
- A run that outlasts the wait answers RUNNING with its run:N; `itse show run:N` follows it.
- NEEDS_HUMAN and UNTRUSTED name the command a human runs in a terminal.
- Exit codes: 0 ok or an empty listing, 1 the verb reports failure (a name lookup that found nothing too), 2 error, 3 a human acts next, 4 running, 130 interrupted.

## Workspace specifics

`itse verbs` lists this workspace's declared actions, its extensions' notes, and the debug verbs where a debugger is declared.
",
    ),
];

/// A template from `TEXT` with its fields filled.
pub fn text(key: &str, vars: &[(&str, &dyn Display)]) -> String {
    let template = TEXT.iter().find(|(k, _)| *k == key).map_or(key, |(_, t)| t);
    vars.iter().fold(template.to_string(), |s, (k, v)| s.replace(&format!("{{{k}}}"), &v.to_string()))
}

/// Page caps as (lines, chars): a verb's answer, a stored log seen through `show`, and a hook's text.
pub const VERB: (usize, usize) = (40, 4000);
pub const SHOW: (usize, usize) = (200, 16_000);
pub const HOOK: (usize, usize) = (usize::MAX, 2000);
/// Detail lines an item shows before pointing at the rest.
pub const DETAIL: usize = 6;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Outcome {
    pub verb: String,
    pub verdict: Verdict,
    pub scope: String,
    pub counts: String,
    pub cost: Cost,
    /// `run:14`, `q:88`, `dbg:2`: where the full result is kept.
    pub id: Option<String>,
    pub items: Vec<Item>,
    pub states: Vec<State>,
    pub notes: Vec<String>,
    pub full: Size,
    pub page: Page,
    /// The usage log's error class; never printed.
    #[serde(skip)]
    pub err: Option<&'static str>,
    /// For the usage log too: the servers asked were warm, and (`problems`) they settled in time.
    #[serde(skip)]
    pub warm: Option<bool>,
    #[serde(skip)]
    pub settled: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    #[default]
    Ok,
    Fail,
    Error,
    Empty,
    Running,
    Stopped,
    Exited,
    NeedsHuman,
    NeedsConsent,
    Untrusted,
    Blocked,
    Unready,
}

/// A located finding: E1 W2 (problems), T3 (tests), S1 (symbols), R4 (references), F1 (frames).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub loc: Option<Loc>,
    /// Source lines as (line number, text), read from disk.
    pub src: Vec<(u32, String)>,
    pub msg: String,
    pub detail: Vec<String>,
    /// The servers and tools that reported it, after duplicates merged.
    pub sources: Vec<String>,
}

/// Repo-relative, else `~/…`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Loc {
    pub path: String,
    pub line: u32,
    pub col: u32,
}

/// The ecosystem's per-item row: what state a thing is in, and who acts next with which command.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub item: String,
    pub state: String,
    pub reason: Option<String>,
    /// `agent`, `human` or `halti-owner`.
    pub actor: Option<String>,
    /// A command, never advice: the verb and its tokens, rendered for the caller.
    pub next: Option<Vec<String>>,
    pub hosts: Vec<String>,
    pub detail: Option<Value>,
    pub job: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Cost {
    pub ms: u64,
    pub warm: Option<bool>,
}

/// The full output's size before capping.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Size {
    pub bytes: u64,
    pub lines: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Page {
    pub n: u32,
    pub of: u32,
}

impl Outcome {
    pub fn new(verb: &str, verdict: Verdict, scope: impl Into<String>, counts: impl Into<String>) -> Self {
        Self { verb: verb.into(), verdict, scope: scope.into(), counts: counts.into(), ..Self::default() }
    }

    /// An `ERROR` result; `err` is its class in the usage log.
    pub fn error(verb: &str, err: &'static str, msg: impl Into<String>) -> Self {
        Self { err: Some(err), ..Self::new(verb, Verdict::Error, "", msg) }
    }

    /// `VERDICT verb scope — counts; cost; id`.
    pub fn head(&self) -> String {
        let verdict = serde_json::to_value(self.verdict).ok().and_then(|v| v.as_str().map(str::to_uppercase)).unwrap_or_default();
        let cost = [(self.cost.ms >= 1000).then(|| format!("{:.1} s", self.cost.ms as f64 / 1000.0)), self.cost.warm.map(|w| (if w { "warm" } else { "cold" }).into())];
        let cost = cost.into_iter().flatten().collect::<Vec<String>>().join(", ");
        let tail: Vec<&str> = [self.counts.as_str(), &cost, self.id.as_deref().unwrap_or("")].into_iter().filter(|s| !s.is_empty()).collect();
        let scope = if self.scope.is_empty() { String::new() } else { format!(" {}", self.scope) };
        format!("{verdict} {}{scope}{}", self.verb, if tail.is_empty() { String::new() } else { format!(" — {}", tail.join("; ")) })
    }

    /// One block per state row, item and note, tagged with its kind and index; a block is never
    /// split across pages. `whole` shows every detail line instead of pointing at them.
    fn blocks(&self, via: &str, whole: bool) -> Vec<(char, usize, String)> {
        let mut out = Vec::new();
        for (i, s) in self.states.iter().enumerate() {
            let human = s.actor.as_deref() == Some("human");
            let next = s.next.as_ref().map(|n| format!("next ({}): {}", s.actor.as_deref().unwrap_or("agent"), crate::mcp::next(n, via, human)));
            let hosts = (!s.hosts.is_empty()).then(|| format!("hosts {}", s.hosts.join(", ")));
            let parts: Vec<String> = [s.reason.clone(), hosts, next, s.job.clone()].into_iter().flatten().collect();
            out.push(('s', i, format!("{:<14} {:<20} {}", s.state, s.item, parts.join("; ")).trim_end().to_string()));
        }
        for (i, it) in self.items.iter().enumerate() {
            let loc = it.loc.as_ref().map(|l| format!("{}:{}:{}", l.path, l.line, l.col)).unwrap_or_default();
            let sources = if it.sources.is_empty() { String::new() } else { format!("[{}]", it.sources.join(", ")) };
            let mut lines = vec![[&it.id, &it.kind, &loc, &it.msg, &sources].iter().filter(|s| !s.is_empty()).map(|s| s.as_str()).collect::<Vec<_>>().join(" ")];
            lines.extend(it.src.iter().map(|(n, t)| format!("{n:>7} | {}", clip(t, 160))));
            let shown = if whole { it.detail.len() } else { it.detail.len().min(DETAIL) };
            lines.extend(it.detail[..shown].iter().map(|d| format!("   {d}")));
            if shown < it.detail.len() {
                lines.push(text("hidden", &[("n", &(it.detail.len() - shown)), ("id", &self.id.as_deref().unwrap_or("?")), ("item", &it.id)]));
            }
            out.push(('i', i, lines.join("\n")));
        }
        out.extend(self.notes.iter().enumerate().map(|(i, n)| ('n', i, n.clone())));
        out
    }

    /// Block indices per page: as many whole blocks as fit under `cap`, after the head and page lines.
    fn pages(&self, blocks: &[(char, usize, String)], (max_lines, max_chars): (usize, usize)) -> Vec<Vec<usize>> {
        let base = self.head().len() + 80;
        let mut pages: Vec<(Vec<usize>, usize, usize)> = vec![(vec![], 2, base)];
        for (i, (_, _, b)) in blocks.iter().enumerate() {
            let (l, c) = (b.lines().count(), b.len() + 1);
            if pages.last().is_some_and(|(page, lines, chars)| !page.is_empty() && (lines + l > max_lines || chars + c > max_chars)) {
                pages.push((vec![], 2, base));
            }
            if let Some((page, lines, chars)) = pages.last_mut() {
                (*lines, *chars) = (*lines + l, *chars + c);
                page.push(i);
            }
        }
        pages.into_iter().map(|p| p.0).collect()
    }

    /// Page `n` as text, or as `{"itse":1, …}` with only that page's rows; it sets `page`, and `full`
    /// when nothing set it before. A page past the end is an `Err` with the line to print.
    pub fn render(&mut self, via: &str, n: u32, cap: (usize, usize), as_json: bool) -> Result<String, String> {
        let blocks = self.blocks(via, false);
        let pages = self.pages(&blocks, cap);
        let of = pages.len() as u32;
        let whole = self.whole(via);
        self.full = if self.full.bytes == 0 { Size { bytes: whole.len() as u64, lines: whole.lines().count() as u64 } } else { self.full.clone() };
        let Some(page) = pages.get(n.max(1) as usize - 1) else { return Err(text("last", &[("n", &n), ("of", &of)])) };
        self.page = Page { n: n.max(1), of };
        if as_json {
            let on = |kind: char, i: usize| page.iter().any(|&b| blocks[b].0 == kind && blocks[b].1 == i);
            let o = Outcome { items: kept(&self.items, |i| on('i', i)), states: kept(&self.states, |i| on('s', i)), notes: kept(&self.notes, |i| on('n', i)), ..self.clone() };
            let mut v = serde_json::to_value(o).unwrap_or_default();
            v["itse"] = 1.into();
            return Ok(format!("{v}\n"));
        }
        let (p, id) = (self.page.n, self.id.as_deref().unwrap_or("?"));
        let footer =
            [(p < of).then(|| text("more", &[("n", &p), ("of", &of), ("id", &id), ("next", &(p + 1))])), (p == of && of > 1).then(|| text("last", &[("n", &p), ("of", &of)]))];
        let lines: Vec<String> = [self.head()].into_iter().chain(page.iter().map(|&b| blocks[b].2.clone())).chain(footer.into_iter().flatten()).collect();
        Ok(lines.join("\n") + "\n")
    }

    /// The whole text, every detail line included and no page cut: what `--grep`, `--tail` and `--raw` read.
    fn whole(&self, via: &str) -> String {
        format!("{}\n{}", self.head(), self.blocks(via, true).into_iter().map(|b| b.2).collect::<Vec<_>>().join("\n"))
    }
}

fn kept<T: Clone>(all: &[T], keep: impl Fn(usize) -> bool) -> Vec<T> {
    all.iter().enumerate().filter(|(i, _)| keep(*i)).map(|(_, x)| x.clone()).collect()
}

/// A line as printed: tabs as 4 spaces, at most `max` chars, the last one `…` when cut.
pub fn clip(line: &str, max: usize) -> String {
    let line = line.replace('\t', "    ");
    if line.chars().count() <= max { line } else { line.chars().take(max - 1).collect::<String>() + "…" }
}

/// A workspace's stored results under its state dir: `results/<n>.json` for queries,
/// `runs/<n>/result.json` for actions and `debug/<n>/result.json` for debug sessions. Kept: the
/// newest 200 queries or 512 MiB.
pub struct Store(pub PathBuf);

impl Store {
    /// Keep a `q:N` result with the session key that asked for it, then drop the oldest past 200
    /// results or 512 MiB.
    pub fn save(&self, o: &Outcome, session: &str) -> Result<()> {
        let path = self.path(o.id.as_deref().unwrap_or("")).ok_or_else(|| anyhow!("no id to store {} under", o.verb))?;
        sys::private_dir(&self.0.join("results"))?;
        let mut v = serde_json::to_value(o)?;
        (v["itse"], v["session"]) = (1.into(), session.into());
        sys::write_atomic(&path, v.to_string().as_bytes())?;
        let mut total = 0;
        for (i, (path, size)) in self.list().into_iter().enumerate() {
            total += size;
            if i >= 200 || total > 512 << 20 {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        match id.split_once(':').filter(|(_, n)| n.parse::<u64>().is_ok())? {
            ("q", n) => Some(self.0.join("results").join(format!("{n}.json"))),
            ("run", n) => Some(self.0.join("runs").join(n).join("result.json")),
            // A debug session's last snapshot, which outlives the session.
            ("dbg", n) => Some(self.0.join("debug").join(n).join("result.json")),
            _ => None,
        }
    }

    /// A stored result and the session key it was stored for; a run's as it is now until it ends.
    pub fn load(&self, id: &str) -> Result<(Outcome, String)> {
        // A run without its result yet (still going, or ended while no daemon ran) reads from its run.json.
        let run = self.path(id).filter(|p| id.starts_with("run:") && !p.exists()).and_then(|p| Some(p.parent()?.to_path_buf())).filter(|d| d.join("run.json").exists());
        if let Some(dir) = run {
            // The runner writes its exit before the daemon parses the output, so give a just-ended
            // run up to 2 s to get its result.json before it's shown unparsed.
            let settled = || dir.join("result.json").exists() || crate::actions::RunRecord::load(&dir).is_none_or(|r| r.exit.is_none());
            sys::wait_until(std::time::Duration::from_secs(2), settled);
            if !dir.join("result.json").exists() {
                return Ok((crate::actions::outcome("run", id, &dir), String::new()));
            }
        }
        let bytes = self.path(id).and_then(|p| fs::read(p).ok()).ok_or_else(|| anyhow!("{}", text("unknown_id", &[("id", &id)])))?;
        let v: Value = serde_json::from_slice(&bytes).with_context(|| format!("reading {id}"))?;
        Ok((serde_json::from_value(v.clone()).with_context(|| format!("reading {id}"))?, v["session"].as_str().unwrap_or("").into()))
    }

    /// The numbers of the runs kept, newest first.
    pub fn runs(&self) -> Vec<u64> {
        let mut runs: Vec<u64> = fs::read_dir(self.0.join("runs")).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
        runs.sort_unstable_by(|a, b| b.cmp(a));
        runs
    }

    /// The stored query results, newest first: `(path, bytes)`.
    fn list(&self) -> Vec<(PathBuf, u64)> {
        let entries = fs::read_dir(self.0.join("results")).into_iter().flatten().flatten();
        let mut all: Vec<(u64, PathBuf, u64)> = entries.filter_map(|e| Some((e.path().file_stem()?.to_str()?.parse().ok()?, e.path(), e.metadata().ok()?.len()))).collect();
        all.sort_by(|a, b| b.0.cmp(&a.0));
        all.into_iter().map(|(_, path, size)| (path, size)).collect()
    }

    /// The newest result holding item `item`: this session's first, else any, so a bare `T1` means
    /// the T1 this caller last saw.
    pub fn holding(&self, item: &str, session: &str) -> Option<(String, Outcome)> {
        let ids = self.list().into_iter().filter_map(|(path, _)| Some(format!("q:{}", path.file_stem()?.to_str()?)));
        let found: Vec<(String, (Outcome, String))> =
            ids.filter_map(|id| Some((id.clone(), self.load(&id).ok()?))).filter(|(_, (o, _))| o.items.iter().any(|i| i.id == item)).collect();
        let pick = found.iter().position(|(_, (_, s))| s == session).unwrap_or(0);
        found.into_iter().nth(pick).map(|(id, (o, _))| (id, o))
    }
}

/// `show <id> [--page N] [--raw] [--grep RE] [--tail N] [--json]`: a stored result by id (`q:7`,
/// `run:14`, `q:7/T1`, or a bare `T1`), paged as its verb paged it, so its pointers lead on. Its
/// whole text (a run's log, say) can be filtered, tailed, or printed raw without a cap.
pub fn show(store: &Store, args: &Args, via: &str, session: &str) -> Answer {
    let target = args.target.clone().unwrap_or_default();
    let (id, item) = target.split_once('/').map_or((target.as_str(), None), |(id, item)| (id, Some(item)));
    let (id, item) = if id.contains(':') { (id, item) } else { ("", Some(id)) };
    let flag = |k: &str| args.flags.get(k).map(|v| v.as_str().map_or(v.to_string(), String::from));
    let fail = |err, msg: String| Answer::failed(Outcome { scope: target.clone(), ..Outcome::error("show", err, msg) });
    let found = if id.is_empty() { store.holding(&target, session) } else { store.load(id).ok().map(|(o, _)| (id.to_string(), o)) };
    let Some((id, mut o)) = found else { return fail("unknown_id", text(if id.is_empty() { "no_item" } else { "unknown_id" }, &[("id", &target)])) };
    let (Ok(page), Ok(tail)) = (flag("page").map_or(Ok(1), |p| p.parse::<u32>()), flag("tail").map(|t| t.parse::<usize>()).transpose()) else {
        return fail("bad_args", "--page and --tail take a number".into());
    };
    if let Some(item) = item {
        o.items.retain(|i| i.id == item);
        (o.states, o.notes) = (vec![], if target.contains(':') { vec![] } else { vec![text("picked", &[("item", &item), ("id", &id)])] });
        if o.items.is_empty() {
            return fail("unknown_id", text("no_item", &[("id", &item)]));
        }
    }
    let (json, raw) = (args.flags.contains_key("json"), args.flags.contains_key("raw"));
    if (item.is_none() || json) && flag("grep").is_none() && tail.is_none() && !raw {
        let full = o.full.bytes;
        let text = o.render(via, page, VERB, json);
        return text.map_or_else(|line| fail("bad_args", line), |text| Answer { verdict: Verdict::Ok, trunc: o.page.of > 1, full, text, ..Answer::default() });
    }
    let whole = o.whole(via);
    let all: Vec<String> = whole.lines().enumerate().map(|(i, l)| if flag("grep").is_some() { format!("{}: {l}", i + 1) } else { l.to_string() }).collect();
    let (counts, mut lines) = match (flag("grep").map(|re| regex_lite::Regex::new(&re)), tail) {
        (Some(Err(e)), _) => return fail("bad_args", format!("--grep: {e}")),
        (Some(Ok(re)), _) => {
            let hits: Vec<String> = all.iter().filter(|l| re.is_match(l.split_once(": ").map_or("", |l| l.1))).cloned().collect();
            (text("grep", &[("n", &hits.len()), ("of", &all.len())]), hits)
        }
        (None, Some(n)) => (text("tail", &[("n", &n.min(all.len())), ("of", &all.len())]), all[all.len().saturating_sub(n)..].to_vec()),
        (None, None) => (String::new(), all.clone()),
    };
    // Whole lines while both caps hold, one line kept for the pointer to the rest.
    let ends = lines.iter().scan(0, |chars, l| Some(*chars + l.len() + 1).inspect(|end| *chars = *end));
    let keep = if raw { lines.len() } else { ends.take(SHOW.0 - 1).take_while(|end| *end < SHOW.1).count() };
    let trunc = keep < lines.len();
    if trunc {
        lines[keep] = text("capped", &[("n", &(lines.len() - keep)), ("id", &id)]);
        lines.truncate(keep + 1);
    }
    let head = Outcome { id: Some(id.clone()), ..Outcome::new("show", Verdict::Ok, "", counts) };
    let head = if head.counts.is_empty() { String::new() } else { head.head() + "\n" };
    Answer { verdict: Verdict::Ok, trunc, full: whole.len() as u64, text: format!("{head}{}\n", lines.join("\n")), ..Answer::default() }
}

#[cfg(test)]
mod tests;
