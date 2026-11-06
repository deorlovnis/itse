//! `itse-bench report`: one section per harness, never compared with each other. Per section: pass
//! rates; per task the pairs of one run's rep (both, only `with`, only `without`, neither) and an
//! exact sign test, controls apart; medians per task and per solved task; itse calls per `with` trial.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

/// Printed under every report: the power of the default design, so a null result isn't over-read.
pub const POWER: &str = "10 tasks x 3 reps detect only large effects; a p above 0.05 is no evidence that the arms are alike.";
pub const TOKENS: &str = "tokens and cost compare only within one harness; est$ is the harness's own price estimate, and a subscription trial spends no dollars.";

struct Row {
    run: String,
    task: String,
    sha: String,
    control: bool,
    rep: u64,
    with: bool,
    pass: bool,
    wall_s: f64,
    /// Fresh and cache-write input tokens; cache reads, which dwarf them, are left out.
    input: Option<f64>,
    output: Option<f64>,
    cost: Option<f64>,
    calls: Option<f64>,
    leftovers: bool,
}

#[derive(Default)]
struct Section {
    versions: BTreeSet<String>,
    rows: Vec<Row>,
    errors: usize,
}

pub fn report(paths: &[PathBuf]) -> Result<String> {
    let mut sections: BTreeMap<String, Section> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let file = if path.is_dir() { path.join("results.jsonl") } else { path.clone() };
        let text = fs::read_to_string(&file).with_context(|| format!("results {}", file.display()))?;
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).with_context(|| format!("{}:{}", file.display(), n + 1))?;
            let field = |k: &str| v.get(k).filter(|x| !x.is_null()).with_context(|| format!("{}:{}: no {k}", file.display(), n + 1));
            let (harness, arm) = (field("harness")?.as_str().unwrap_or_default().to_string(), field("arm")?.as_str().unwrap_or_default());
            let sum = |keys: &[&str]| keys.iter().filter_map(|k| v[*k].as_f64()).reduce(|a, b| a + b);
            let row = Row {
                run: v["run"].as_str().unwrap_or_default().into(),
                task: field("task")?.as_str().unwrap_or_default().into(),
                sha: v["task_sha"].as_str().unwrap_or_default().into(),
                control: v["tags"].as_array().is_some_and(|t| t.contains(&"control".into())),
                rep: field("rep")?.as_u64().unwrap_or_default(),
                with: arm == "with",
                pass: field("pass")? == true,
                wall_s: field("wall_ms")?.as_f64().unwrap_or_default() / 1000.0,
                input: sum(&["fresh_tokens", "cache_write_tokens"]),
                output: v["output_tokens"].as_f64(),
                cost: v["cost_usd"].as_f64(),
                calls: v["itse"]["usage"]["calls"].as_f64(),
                leftovers: v["leftovers"].as_array().is_some_and(|l| !l.is_empty()),
            };
            let new = seen.insert((harness.clone(), row.run.clone(), row.task.clone(), row.rep, row.with));
            ensure!(new, "{}:{}: run '{}' has {} rep {} {arm} twice", file.display(), n + 1, row.run, row.task, row.rep);
            let error = harness_error(&v);
            let section = sections.entry(harness).or_default();
            section.versions.insert(v["harness_version"].as_str().unwrap_or("?").to_string());
            section.errors += usize::from(error);
            (!error).then(|| section.rows.push(row));
        }
    }
    ensure!(!sections.is_empty(), "no results in {}", paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
    let body: Vec<String> = sections.iter_mut().map(|(harness, s)| section(harness, s)).collect();
    Ok(format!("{}\n{POWER}\n{TOKENS}\n", body.join("\n")))
}

fn section(harness: &str, s: &mut Section) -> String {
    // A task whose text changed between results is two tasks, told apart by its sha.
    let shas: BTreeSet<(&str, &str)> = s.rows.iter().map(|r| (r.task.as_str(), r.sha.as_str())).collect();
    let mixed: BTreeSet<String> = shas.iter().filter(|(t, _)| shas.iter().filter(|(u, _)| u == t).count() > 1).map(|(t, _)| t.to_string()).collect();
    s.rows.iter_mut().filter(|r| mixed.contains(&r.task)).for_each(|r| r.task = format!("{}@{:.8}", r.task, r.sha));
    s.rows.iter_mut().filter(|r| r.control).for_each(|r| r.task += " (control)");
    let rows = &s.rows;
    let tasks: BTreeMap<&str, bool> = rows.iter().map(|r| (r.task.as_str(), r.control)).collect();
    let versions = s.versions.iter().cloned().collect::<Vec<_>>().join(", ");
    let mut out = format!("harness {harness} ({versions}): {} trials, {} tasks\n", rows.len() + s.errors, tasks.len());
    let notes = [
        (s.errors, "harness errors (exit not 0 or is_error, no tokens, no timeout), left out"),
        (rows.iter().filter(|r| r.leftovers).count(), "trials left processes behind (killed)"),
    ];
    out += &notes.iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}\n")).collect::<String>();
    let rate = |with: bool, control: bool| {
        let arm: Vec<&Row> = rows.iter().filter(|r| r.with == with && r.control == control).collect();
        let passed = arm.iter().filter(|r| r.pass).count();
        let share = (!arm.is_empty()).then(|| 100.0 * passed as f64 / arm.len() as f64);
        format!("{passed}/{} ({})", arm.len(), share.map_or("-".into(), |s| format!("{s:.0}%")))
    };
    let controls = tasks.values().any(|c| *c);
    let rest = if controls { format!("; controls with {}, without {}", rate(true, true), rate(false, true)) } else { String::new() };
    out += &format!("pass rate: with {}, without {}{rest}\n\n", rate(true, false), rate(false, false));

    let width = tasks.keys().map(|t| t.len()).max().unwrap_or(0).max(8);
    let row = |name: &str, c: [u64; 4]| format!("{name:<width$}  {:>5}  {:>9}  {:>12}  {:>7}\n", c[0], c[1], c[2], c[3]);
    out += &format!("{:<width$}  {:>5}  {:>9}  {:>12}  {:>7}\n", "paired", "both", "only with", "only without", "neither");
    // Per kind (tasks, controls): both, only with, only without, neither.
    let mut all = [[0; 4]; 2];
    for (task, control) in &tasks {
        let mut counts = [0; 4];
        let reps: BTreeSet<(&str, u64)> = rows.iter().filter(|r| r.task == *task).map(|r| (r.run.as_str(), r.rep)).collect();
        for (run, rep) in reps {
            let pass = |with: bool| rows.iter().find(|r| r.task == *task && r.run == run && r.rep == rep && r.with == with).map(|r| r.pass);
            if let (Some(w), Some(wo)) = (pass(true), pass(false)) {
                counts[2 * usize::from(!w) + usize::from(!wo)] += 1;
            }
        }
        all[usize::from(*control)].iter_mut().zip(counts).for_each(|(a, c)| *a += c);
        out += &row(task, counts);
    }
    for (name, of, c) in [("all", "", all[0]), ("controls", " of the controls", all[1])].iter().take(1 + usize::from(controls)) {
        out += &row(name, *c);
        out += &format!("sign test on {} discordant pairs{of} ({} only with, {} only without): p = {:.4}\n", c[1] + c[2], c[1], c[2], sign_test(c[1], c[2]));
    }

    let cols = |name: &str, label: &str, cells: &[String]| {
        let body: String = cells.chunks(2).zip([8, 9, 8, 9]).map(|(c, w)| format!("  {:>w$} {:>w$}", c[0], c[1])).collect();
        format!("{name:<width$}  {label:<10}{body}\n")
    };
    out += "\n";
    out += &cols("medians", "", &["s with", "s w/o", "in with", "in w/o", "out with", "out w/o", "est$ with", "est$ w/o"].map(String::from));
    for task in tasks.keys() {
        for (label, solved) in [("per task", false), ("per solved", true)] {
            let of = |with: bool, value: fn(&Row) -> Option<f64>, digits: usize| {
                show(median(rows.iter().filter(|r| r.task == *task && r.with == with && (r.pass || !solved)).filter_map(value).collect()), digits)
            };
            let pair = |value: fn(&Row) -> Option<f64>, digits| [of(true, value, digits), of(false, value, digits)];
            let cells = [pair(|r| Some(r.wall_s), 1), pair(|r| r.input, 0), pair(|r| r.output, 0), pair(|r| r.cost, 4)].concat();
            out += &cols(if solved { "" } else { task }, label, &cells);
        }
    }
    let calls: Vec<f64> = rows.iter().filter(|r| r.with).filter_map(|r| r.calls).collect();
    let max = calls.iter().copied().reduce(f64::max);
    out + &format!("itse calls per with trial: median {}, max {} ({} trials)\n", show(median(calls.clone()), 1), show(max, 0), calls.len())
}

/// Exit not 0 (or is_error) with no tokens and no timeout: the harness never worked on the task (it could not log in, say), so no failure of the agent.
pub fn harness_error(v: &Value) -> bool {
    let tokens = ["fresh_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens"].iter().any(|k| v[*k].as_f64().is_some_and(|x| x > 0.0));
    (v["exit"].as_i64().is_some_and(|e| e != 0) || v["is_error"] == true) && v["timed_out"] != true && !tokens
}

/// The exact two-sided sign test on `b` pairs one way and `c` the other: p = min(1, 2 · Σ_{k ≤ min(b, c)} C(n, k) / 2ⁿ) with n = b + c; 1 without discordant pairs.
pub fn sign_test(b: u64, c: u64) -> f64 {
    let n = b + c;
    // ln C(n, k) term by term, C(n, k + 1) = C(n, k) · (n − k) / (k + 1), summed relative to the largest (the last, as k ≤ n / 2), so 2⁻ⁿ never underflows on its own.
    let ln: Vec<f64> = (0..=b.min(c)).scan(0.0, |acc, k| Some(std::mem::replace(acc, *acc + ((n - k) as f64 / (k + 1) as f64).ln()))).collect();
    let top = ln[ln.len() - 1];
    let sum: f64 = ln.iter().map(|l| (l - top).exp()).sum();
    (2.0 * (top + sum.ln() - n as f64 * 2f64.ln()).exp()).min(1.0)
}

pub fn median(mut v: Vec<f64>) -> Option<f64> {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    (n > 0).then(|| if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

fn show(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |v| format!("{v:.digits$}"))
}

#[cfg(test)]
#[path = "../tests/unit/report.rs"]
mod tests;
