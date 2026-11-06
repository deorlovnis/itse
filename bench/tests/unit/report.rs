use std::{env, process};

use serde_json::{Value, json};

use super::*;

#[test]
fn the_sign_test_matches_known_binomial_values() {
    let close = |b, c, want: f64| assert!((sign_test(b, c) - want).abs() < 1e-12, "b={b} c={c}: {} != {want}", sign_test(b, c));
    close(0, 0, 1.0);
    close(0, 5, 2.0 / 32.0);
    close(5, 0, 0.0625);
    close(1, 9, 22.0 / 1024.0);
    close(2, 10, 2.0 * 79.0 / 4096.0);
    close(5, 5, 1.0);
    close(3, 4, 1.0);
    close(0, 30, 2.0 * 0.5f64.powi(30));
    close(0, 6, 0.03125);
}

#[test]
fn the_sign_test_holds_up_past_a_thousand_pairs() {
    assert!((sign_test(550, 550) - 1.0).abs() < 1e-9, "{}", sign_test(550, 550));
    let p = sign_test(500, 600);
    assert!(p > 0.0 && p < 0.01, "{p}");
}

#[test]
fn medians_of_odd_even_and_empty_lists() {
    assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
    assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    assert_eq!(median(vec![]), None);
}

fn line(harness: &str, task: &str, rep: u64, arm: &str, pass: bool, wall_ms: u64, tokens: Option<u64>) -> String {
    let calls = if arm == "with" { json!(2) } else { json!(0) };
    json!({"run": "r1", "harness": harness, "harness_version": format!("{harness} 1.0"), "task": task, "rep": rep, "arm": arm, "pass": pass, "wall_ms": wall_ms,
        "fresh_tokens": tokens, "cache_read_tokens": tokens.map(|t| t * 2), "cache_write_tokens": null, "output_tokens": null, "cost_usd": tokens.map(|_| 0.5),
        "itse": {"usage": {"calls": calls}}})
    .to_string()
}

#[test]
fn the_report_pairs_reps_per_task_and_keeps_harnesses_apart() {
    let lines = [
        line("fake", "a", 1, "with", true, 1000, Some(100)),
        line("fake", "a", 1, "without", false, 3000, Some(300)),
        line("fake", "a", 2, "without", true, 2000, None),
        line("fake", "a", 2, "with", true, 1000, Some(100)),
        line("fake", "b", 1, "with", false, 500, None),
        line("fake", "b", 1, "without", false, 700, None),
        line("fake", "b", 2, "with", true, 500, None),
        line("other", "a", 1, "with", true, 9000, Some(5)),
    ];
    let file = env::temp_dir().join(format!("itse-bench-report-{}.jsonl", process::id()));
    fs::write(&file, lines.join("\n") + "\n\n").unwrap();
    let text = report(std::slice::from_ref(&file)).unwrap();
    let _ = fs::remove_file(&file);
    assert!(text.starts_with("harness fake (fake 1.0): 7 trials, 2 tasks\npass rate: with 3/4 (75%), without 1/3 (33%)\n"), "{text}");
    assert!(text.contains("\na             1          1             0        0\n"), "{text}");
    assert!(text.contains("\nb             0          0             0        1\n"), "b's rep 2 has no pair: {text}");
    assert!(text.contains("\nall           1          1             0        1\n"), "{text}");
    assert!(text.contains("sign test on 1 discordant pairs (1 only with, 0 only without): p = 1.0000\n"), "{text}");
    // a: with walls 1.0 and 1.0; without 3.0 and 2.0 (median 2.5); input is fresh + cache write, not cache reads.
    assert!(text.contains("\nmedians                 s with    s w/o    in with    in w/o  out with  out w/o  est$ with  est$ w/o\n"), "{text}");
    assert!(text.contains("\na         per task         1.0      2.5        100       300         -        -     0.5000    0.5000\n"), "{text}");
    assert!(text.contains("\n          per solved       1.0      2.0        100         -         -        -     0.5000         -\n"), "{text}");
    assert!(text.contains("pass rate: with 1/1 (100%), without 0/0 (-)\n"), "{text}");
    assert!(text.contains("itse calls per with trial: median 2.0, max 2 (4 trials)\n"), "{text}");
    assert!(text.contains("\nharness other (other 1.0): 1 trials, 1 tasks\n"), "{text}");
    assert!(text.ends_with(&format!("{POWER}\n{TOKENS}\n")));
}

#[test]
fn a_line_without_a_required_field_is_named() {
    let file = env::temp_dir().join(format!("itse-bench-report-bad-{}.jsonl", process::id()));
    fs::write(&file, "{\"harness\":\"h\",\"arm\":\"with\",\"task\":\"a\",\"rep\":1,\"wall_ms\":1}\n").unwrap();
    let err = format!("{:#}", report(std::slice::from_ref(&file)).err().unwrap());
    let _ = fs::remove_file(&file);
    assert!(err.ends_with(":1: no pass"), "{err}");
}

#[test]
fn runs_appended_to_one_file_all_pair_and_a_repeated_trial_is_refused() {
    let file = env::temp_dir().join(format!("itse-bench-report-runs-{}.jsonl", process::id()));
    let mut lines = vec![];
    for run in ["a", "b"] {
        for rep in 1..=3 {
            for (arm, pass) in [("with", true), ("without", false)] {
                lines.push(json!({"run": run, "harness": "h", "task": "t", "rep": rep, "arm": arm, "pass": pass, "wall_ms": 1}).to_string());
            }
        }
    }
    fs::write(&file, lines.join("\n")).unwrap();
    let text = report(std::slice::from_ref(&file)).unwrap();
    assert!(text.contains("\nt             0          6             0        0\n"), "{text}");
    assert!(text.contains("sign test on 6 discordant pairs (6 only with, 0 only without): p = 0.0313\n"), "{text}");
    let err = format!("{:#}", report(&[file.clone(), file.clone()]).err().unwrap());
    let _ = fs::remove_file(&file);
    assert!(err.ends_with(":1: run 'a' has t rep 1 with twice"), "{err}");
}

#[test]
fn harness_errors_are_left_out_controls_kept_apart_and_changed_tasks_told_apart() {
    let file = env::temp_dir().join(format!("itse-bench-report-kinds-{}.jsonl", process::id()));
    let mut lines = vec![];
    let mut add = |task: &str, sha: &str, rep: u64, arm: &str, pass: bool, more: Value| {
        let mut v = json!({"run": "r", "harness": "h", "task": task, "task_sha": sha, "rep": rep, "arm": arm, "pass": pass, "wall_ms": 1, "exit": 0, "turns": 2});
        v.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
        lines.push(v.to_string());
    };
    for rep in 1..=2 {
        add("t", "aaaaaaaa11", rep, "with", true, json!({}));
        add("t", "aaaaaaaa11", rep, "without", false, json!({}));
        add("d", "dd", rep, "with", true, json!({"tags": ["docs", "control"]}));
        add("d", "dd", rep, "without", false, json!({"tags": ["docs", "control"], "leftovers": [42]}));
    }
    // An edited t: its own row; a harness that could not start: no pair, no failure.
    add("t", "bbbbbbbb22", 3, "with", false, json!({}));
    add("t", "bbbbbbbb22", 3, "without", true, json!({}));
    add("t", "bbbbbbbb22", 4, "with", false, json!({"exit": 7, "turns": 0, "output_tokens": 0}));
    add("t", "bbbbbbbb22", 4, "without", false, json!({}));
    add("t", "bbbbbbbb22", 5, "without", false, json!({"exit": 1, "turns": null, "timed_out": true}));
    // A login failure in Claude Code's JSON: a turn, but no tokens and is_error.
    add("t", "bbbbbbbb22", 6, "with", false, json!({"exit": 1, "turns": 1, "is_error": true, "fresh_tokens": 0, "output_tokens": 0}));
    fs::write(&file, lines.join("\n")).unwrap();
    let text = report(std::slice::from_ref(&file)).unwrap();
    let _ = fs::remove_file(&file);
    assert!(
        text.starts_with(
            "harness h (?): 14 trials, 3 tasks\n2 harness errors (exit not 0 or is_error, no tokens, no timeout), left out\n2 trials left processes behind (killed)\n"
        ),
        "{text}"
    );
    assert!(text.contains("pass rate: with 2/3 (67%), without 1/5 (20%); controls with 2/2 (100%), without 0/2 (0%)\n"), "{text}");
    assert!(text.contains("\nd (control)      0          2             0        0\n"), "{text}");
    assert!(text.contains("\nt@aaaaaaaa       0          2             0        0\n"), "{text}");
    assert!(text.contains("\nt@bbbbbbbb       0          0             1        0\n"), "a harness error pairs with nothing: {text}");
    assert!(text.contains("\nall              0          2             1        0\nsign test on 3 discordant pairs (2 only with, 1 only without): p = 1.0000\n"), "{text}");
    assert!(
        text.contains("\ncontrols         0          2             0        0\nsign test on 2 discordant pairs of the controls (2 only with, 0 only without): p = 0.5000\n"),
        "{text}"
    );
}

#[test]
fn a_harness_error_is_told_by_its_tokens_not_its_turns() {
    let error = |v: Value| harness_error(&v);
    assert!(error(json!({"exit": 1, "turns": 1, "is_error": true, "fresh_tokens": 0, "output_tokens": 0})), "Claude Code's 'Invalid API key' result");
    assert!(error(json!({"exit": 0, "is_error": true})) && error(json!({"exit": 7, "turns": null})));
    assert!(!error(json!({"exit": 1, "turns": 40, "is_error": true, "output_tokens": 900})), "a harness that worked and then failed, at its turn cap say");
    assert!(!error(json!({"exit": 143, "timed_out": true})) && !error(json!({"exit": 0, "turns": 0})));
}
