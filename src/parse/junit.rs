//! The streamed JUnit XML scanner (design §6.4), with no XML crate: a byte loop over a `BufReader`,
//! so memory stays flat whatever a report's size. It tracks `testsuite → testcase → failure | error
//! | skipped`, keeps at most 8 KB of failure text per test case and nothing of `system-out` or
//! `system-err`, skips `<?…?>`, comments and `<!DOCTYPE…>`, takes CDATA as text, and decodes the
//! five named entities and `&#N;`/`&#xH;`. A report that stops making sense is one error item at
//! its byte offset; the other reports still parse.

use std::fs::File;
use std::io::{BufReader, Bytes, Read};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use regex_lite::Regex;

use super::{Parsed, trace};
use crate::manifest::Action;
use crate::results::Item;

/// Failure text kept per test case.
const TEXT_CAP: usize = 8 << 10;

/// One test case: its class and name, `passed`, `failed` or `skipped`, and its failure text.
#[derive(Debug, Default, PartialEq)]
pub struct Case {
    pub class: String,
    pub name: String,
    pub status: &'static str,
    pub text: String,
}

struct Scan<R: Read> {
    bytes: Bytes<BufReader<R>>,
    pos: u64,
    /// The byte just before the last `end` found, kept past `cap`: a long tag's `/` of `/>`.
    before: u8,
}

impl<R: Read> Scan<R> {
    fn next(&mut self) -> Option<u8> {
        self.pos += 1;
        self.bytes.next().and_then(Result::ok)
    }

    /// Read up to and past `end`, keeping what came before it in `keep` (up to `cap` bytes);
    /// false at the end of the file. `quoted` skips an `end` inside quotes, as in a tag.
    fn until(&mut self, end: &[u8], keep: &mut Vec<u8>, cap: usize, quoted: bool) -> bool {
        let (mut window, mut quote) = (Vec::with_capacity(end.len()), None);
        self.before = 0;
        while let Some(b) = self.next() {
            quote = match (quote, b) {
                (None, b'"' | b'\'') if quoted => Some(b),
                (Some(q), b) if b == q => None,
                (q, _) => q,
            };
            window.push(b);
            if window.len() > end.len() {
                let b = window.remove(0);
                self.before = b;
                if keep.len() < cap {
                    keep.push(b);
                }
            }
            if quote.is_none() && window == end {
                return true;
            }
        }
        false
    }
}

/// The test cases of one report, or the byte offset where it stops making sense.
pub fn scan(r: impl Read) -> Result<Vec<Case>, u64> {
    let mut s = Scan { bytes: BufReader::new(r).bytes(), pos: 0, before: 0 };
    let (mut cases, mut stack, mut case, mut text, mut root) = (Vec::new(), Vec::<String>::new(), None::<Case>, Vec::new(), false);
    let keeps = |stack: &[String]| stack.last().is_some_and(|t| t == "failure" || t == "error");
    while let Some(b) = s.next() {
        if b != b'<' {
            if keeps(&stack) && text.len() < TEXT_CAP {
                text.push(b);
            }
            continue;
        }
        if let Some(c) = case.as_mut().filter(|c| keeps(&stack) && c.text.len() < TEXT_CAP) {
            c.text.push_str(&decode(&String::from_utf8_lossy(&text)));
        }
        text.clear();
        let (mut tag, mut cdata) = (Vec::new(), Vec::new());
        let ok = match s.next() {
            Some(b'?') => s.until(b"?>", &mut tag, 0, false),
            Some(b'!') => match [s.next(), s.next()] {
                [Some(b'-'), Some(b'-')] => s.until(b"-->", &mut tag, 0, false),
                [Some(b'['), Some(b'C')] => s.until(b"[", &mut tag, 0, false) && s.until(b"]]>", &mut cdata, TEXT_CAP, false),
                _ => s.until(b">", &mut tag, 0, true),
            },
            Some(b) => {
                tag.push(b);
                s.until(b">", &mut tag, 16 << 10, true)
            }
            None => false,
        };
        if let Some(c) = case.as_mut().filter(|c| keeps(&stack) && c.text.len() < TEXT_CAP) {
            c.text.push_str(&String::from_utf8_lossy(&cdata));
        }
        let tag = String::from_utf8_lossy(&tag).into_owned();
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_string();
        if !ok || name.is_empty() && !tag.is_empty() {
            return Err(s.pos);
        }
        if tag.starts_with('/') {
            if stack.pop() != Some(name.clone()) {
                return Err(s.pos);
            }
            if name == "testcase" {
                cases.extend(case.take());
            }
            continue;
        }
        if name.is_empty() {
            continue;
        }
        root |= name == "testsuite" || name == "testsuites";
        match name.as_str() {
            "testcase" => {
                case = Some(Case { class: attr(&tag, "classname").unwrap_or_default(), name: attr(&tag, "name").unwrap_or_default(), status: "passed", text: String::new() })
            }
            "failure" | "error" | "skipped" => {
                if let Some(c) = case.as_mut().filter(|c| c.status != "failed") {
                    c.status = if name == "skipped" { "skipped" } else { "failed" };
                    c.text = attr(&tag, "message").filter(|_| name != "skipped").map(|m| m + "\n").unwrap_or_default();
                }
            }
            _ => {}
        }
        if s.before == b'/' {
            cases.extend((name == "testcase").then(|| case.take()).flatten());
        } else {
            stack.push(name);
        }
    }
    if !stack.is_empty() || !root { Err(s.pos) } else { Ok(cases) }
}

/// An attribute's value, decoded.
fn attr(tag: &str, key: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(i) = rest.find('=') {
        let name = rest[..i].trim_end().rsplit(char::is_whitespace).next().unwrap_or("");
        let value = rest[i + 1..].trim_start();
        let quote = value.chars().next().filter(|q| *q == '"' || *q == '\'')?;
        let end = value[1..].find(quote)? + 1;
        if name == key {
            return Some(decode(&value[1..end]));
        }
        rest = &value[end + 1..];
    }
    None
}

/// Text with `&lt; &gt; &amp; &quot; &apos; &#N; &#xH;` decoded; anything else is left as it is.
pub fn decode(s: &str) -> String {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let (entity, tail) = rest[i + 1..].split_once(';').filter(|(e, _)| e.len() <= 8).unwrap_or(("", ""));
        let named = [("lt", '<'), ("gt", '>'), ("amp", '&'), ("quot", '"'), ("apos", '\'')].iter().find(|(n, _)| *n == entity).map(|(_, c)| *c);
        let number = entity.strip_prefix("#x").map(|h| u32::from_str_radix(h, 16)).or_else(|| entity.strip_prefix('#').map(str::parse)).and_then(Result::ok);
        match named.or(number.and_then(char::from_u32)) {
            Some(c) => (out, rest) = (out + &c.to_string(), tail),
            None => (out, rest) = (out + "&", &rest[i + 1..]),
        }
    }
    out + rest
}

/// The reports under `root` matching `glob` (`**/` any directories, `*` within a name): those
/// written by this run (mtime ≥ its start − 1 s), else those in the directory of the newest one
/// (a test task rewrites its directory whole), marked as written before it. Their failed cases
/// become T items with trimmed traces; their counts add to `out`.
pub fn reports(root: &Path, glob: &str, started: SystemTime, a: &Action, index: &mut trace::Index, out: &mut Parsed) -> Result<()> {
    let re = glob.split("**/").map(|part| part.split('*').map(regex_lite::escape).collect::<Vec<_>>().join("[^/]*")).collect::<Vec<_>>().join("(?:.*/)?");
    let re = Regex::new(&format!("^{re}$"))?;
    let (files, _) = trace::walk(root, &[".git", "node_modules"], 200_000);
    let files: Vec<(String, SystemTime)> =
        files.into_iter().filter(|f| re.is_match(f)).filter_map(|f| Some((f.clone(), root.join(&f).metadata().ok()?.modified().ok()?))).collect();
    let fresh: Vec<&(String, SystemTime)> = files.iter().filter(|(_, t)| *t + Duration::from_secs(1) >= started).collect();
    let dir = |p: &str| p.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
    let newest = files.iter().max_by_key(|(_, t)| *t).map(|(p, _)| dir(p));
    let stale = fresh.is_empty() && !files.is_empty();
    let read = if stale { files.iter().filter(|(p, _)| Some(dir(p)) == newest).collect() } else { fresh };
    let note = if stale { format!("reports: {} files, report written before this run", read.len()) } else { format!("reports: {} files written by this run", read.len()) };
    out.notes.extend((!files.is_empty()).then_some(note));
    for (path, _) in read {
        let cases = match File::open(root.join(path)).map_err(|_| 0).and_then(scan) {
            Ok(cases) => cases,
            Err(at) => {
                out.items.push(Item { kind: "error".into(), msg: format!("unparsed report {path} at byte {at}"), sources: vec!["junit-xml".into()], ..Item::default() });
                continue;
            }
        };
        let sum = out.counts.get_or_insert([0; 3]);
        for c in cases {
            let i = ["passed", "failed", "skipped"].iter().position(|s| *s == c.status).unwrap_or(0);
            sum[i] += 1;
            if c.status == "failed" {
                let (detail, loc, cut) = trace::trim(&c.text, &c.class, a.frames.as_ref(), &a.classify, index);
                out.notes.extend(cut.then(|| format!("index_truncated: more than {} files; some frames are unlocated", trace::INDEX_CAP)));
                let src = loc.as_ref().and_then(|l| super::source(root, &l.path, l.line)).into_iter().collect();
                out.items.push(Item { kind: "failed".into(), msg: format!("{} > {}", c.class, c.name), loc, src, detail, ..Item::default() });
            }
        }
    }
    out.notes.dedup();
    Ok(())
}
