use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::manifest::{BUILT_INS, parse_ext};
use crate::sys::tests::TempDir;

const TEST_FAILURES: &str = include_str!("../../tests/fixtures/cargo-1.92/test-failures.txt");
const COMPILE_ERROR: &str = include_str!("../../tests/fixtures/cargo-1.92/test-compile-error.txt");
const SPAWNED_PANIC: &str = include_str!("../../tests/fixtures/cargo-1.92/test-spawned-panic.txt");

fn rust_ws() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust-ws")
}

fn rust() -> Extension {
    parse_ext(BUILT_INS.iter().find(|b| b.0 == "rust").unwrap().1, "rust").unwrap()
}

/// `cargo:test` of the built-in rust extension over `log`, with `root` as the workspace.
fn cargo_test(root: &Path, log: &str) -> Parsed {
    let (tmp, ext) = (TempDir::new("itse-parse"), rust());
    fs::write(tmp.0.join("out.log"), log).unwrap();
    let named = ext.problem_matchers.iter().map(|(k, m)| (k.as_str(), m)).collect();
    let a = ext.actions.iter().find(|a| a.label == "cargo:test").unwrap();
    run(root, a, &named, &tmp.0.join("out.log"), SystemTime::now()).unwrap()
}

/// What `problems` gets from servers that all settled and pushed nothing.
fn settled() -> Pushed {
    Pushed { settled: true, ..Pushed::default() }
}

fn lines(it: &Item) -> String {
    let loc = it.loc.as_ref().map_or(String::new(), |l| format!("{}:{}:{}", l.path, l.line, l.col));
    let src = it.src.iter().map(|(n, t)| format!("\n{n} | {t}")).collect::<String>();
    format!("{} {} {loc} {}{src}\n{}", it.id, it.kind, it.msg, it.detail.join("\n"))
}

#[test]
fn cargo_test_reports_the_failing_test_at_its_line_with_source() {
    let p = cargo_test(&rust_ws(), TEST_FAILURES);
    assert_eq!(
        lines(&p.items[0]),
        "T1 failed src/lib.rs:25:9 tests::saves_every_line\n25 |         assert_eq!(saved_lines(&[1, 2, 3]), 3);\nassertion `left == right` failed\n  left: 2\n right: 3"
    );
    assert!(p.items[0].sources.is_empty(), "a test item names no tool");
    assert_eq!(p.notes, ["doctests not run"], "every cargo test result says doctests didn't run");
}

#[test]
fn cargo_test_runs_every_test_binary_after_a_failure() {
    let p = cargo_test(&rust_ws(), TEST_FAILURES);
    assert_eq!(p.counts, Some([2, 2, 1]), "both test binaries' summaries are summed");
    let ids: Vec<(&str, &str)> = p.items.iter().map(|i| (i.id.as_str(), i.loc.as_ref().unwrap().path.as_str())).collect();
    assert_eq!(ids, [("T1", "src/lib.rs"), ("T2", "tests/orders.rs")]);
    let mut o = Outcome::new("test", Verdict::Fail, "rust-ws (cargo:test)", "exit 101");
    o.notes = vec!["error: 2 targets failed:".into(), "log: /s/runs/1/out.log".into()];
    p.apply(&mut o);
    assert_eq!(o.head(), "FAIL test rust-ws (cargo:test) — 2 failed, 2 passed, 1 skipped");
    assert_eq!(o.notes, ["doctests not run", "log: /s/runs/1/out.log"], "the items replace the log's tail");
}

#[test]
fn a_panic_line_without_pid_still_matches() {
    let older = Regex::new(r"' \(\d+\) panicked").unwrap().replace_all(TEST_FAILURES, "' panicked");
    assert!(!older.contains("(51984)") && older.contains("thread 'tests::saves_every_line' panicked at src/lib.rs:25:9:"));
    let p = cargo_test(&rust_ws(), &older);
    assert_eq!(p.items.iter().map(lines).collect::<Vec<_>>(), cargo_test(&rust_ws(), TEST_FAILURES).items.iter().map(lines).collect::<Vec<_>>());
}

#[test]
fn the_backtrace_note_is_not_a_detail_line() {
    let p = cargo_test(&rust_ws(), TEST_FAILURES);
    assert_eq!(p.items[1].detail, ["an order needs lines", "second line of the message"], "the message's lines, then the blank line ends it");
    assert!(p.items.iter().flat_map(|i| &i.detail).all(|d| !d.contains("RUST_BACKTRACE")));
    // Without drop_lines the note would be the fourth line of T1.
    assert_eq!(p.items[0].detail.len(), 3);
}

#[test]
fn a_panic_on_a_thread_the_test_spawned_is_not_a_test() {
    let tmp = TempDir::new("itse-parse");
    let p = cargo_test(&tmp.0, SPAWNED_PANIC);
    let tests: Vec<(&str, &str)> = p.items.iter().map(|i| (i.id.as_str(), i.msg.as_str())).collect();
    assert_eq!((tests, p.counts), (vec![("T1", "tests::worker_result_is_used")], Some([0, 1, 0])), "the '<unnamed>' thread's panic is no second test");
}

#[test]
fn a_compile_error_is_e1_with_the_source_line() {
    let tmp = TempDir::new("itse-parse");
    fs::create_dir_all(tmp.0.join("src")).unwrap();
    fs::copy(rust_ws().join("src/broken.rs.txt"), tmp.0.join("src/lib.rs")).unwrap();
    let p = cargo_test(&tmp.0, COMPILE_ERROR);
    assert_eq!(p.items.len(), 1, "the error printed for lib and lib test is one item: {:?}", p.items);
    assert_eq!(lines(&p.items[0]), "E1 error src/lib.rs:6:7 E0277: cannot add `&str` to `u32`: no implementation for `u32 + &str`\n6 |     a + \"b\"\n");
    assert_eq!((p.items[0].sources.clone(), p.counts), (vec!["rustc".to_string()], None));
}

#[test]
fn lookbehind_matchers_are_refused_at_load() {
    for re in [r"(?<=error) (.*)", r"(a)\1", r"x(?=y)"] {
        let ext = json!({"itse_ext": 1, "id": "t", "problemMatchers": {"m": {"pattern": {"regexp": re, "message": 1}}}});
        let err = format!("{:#}", parse_ext(&ext.to_string(), "t.json").unwrap_err());
        assert!(err.starts_with("extension t.json: problemMatcher: regex '") && err.contains("is refused (regex-lite has no lookaround or backreferences)"), "{err}");
    }
    let ext = json!({"itse_ext": 1, "id": "t", "actions": [{"label": "t:x", "command": "x", "drop_lines": ["(?<!a)b"]}]});
    assert!(format!("{:#}", parse_ext(&ext.to_string(), "t.json").unwrap_err()).contains("t:x: regex '(?<!a)b' is refused"));
    let ext = json!({"itse_ext": 1, "id": "t", "problemMatchers": {"m": {"pattern": [{"regexp": "a", "loop": true}]}}});
    assert!(parse_ext(&ext.to_string(), "t.json").is_err(), "no loop patterns yet");
}

#[test]
fn a_one_line_matcher_with_an_absolute_file_location_makes_paths_repo_relative() {
    let m: Matcher = serde_json::from_value(json!({"owner": "kotlinc", "fileLocation": "absolute",
        "pattern": {"regexp": "^([ew]): file://([^:]+):(\\d+):(\\d+) (.*)$", "severity": 1, "file": 2, "line": 3, "column": 4, "message": 5}}))
    .unwrap();
    let (root, mut items) = (Path::new("/w"), Vec::new());
    let mut live = Live {
        m: &m,
        tool: "kotlinc".into(),
        patterns: vec![(Regex::new(&m.pattern.as_ref().unwrap().as_slice()[0].regexp).unwrap(), &m.pattern.as_ref().unwrap().as_slice()[0])],
        open: None,
    };
    for line in ["w: file:///w/data/A.kt:3:9 Variable 'tmp' is never used", "BUILD FAILED", "e: file:///elsewhere/B.kt:1:1 Unresolved reference 'x'"] {
        live.feed(root, line, &mut items);
    }
    let got: Vec<(String, String, u32, String)> =
        items.iter().map(|i| (i.kind.clone(), i.loc.as_ref().unwrap().path.clone(), i.loc.as_ref().unwrap().line, i.msg.clone())).collect();
    assert_eq!(
        got,
        [("warning".into(), "data/A.kt".into(), 3, "Variable 'tmp' is never used".into()), ("error".into(), "/elsewhere/B.kt".into(), 1, "Unresolved reference 'x'".into())]
    );
}

fn item(kind: &str, path: &str, line: u32, msg: &str, src: &str, tool: &str) -> Item {
    let loc = Some(Loc { path: path.into(), line, col: 1 });
    Item { kind: kind.into(), loc, msg: msg.into(), src: vec![(line, src.into())], sources: vec![tool.into()], ..Item::default() }
}

#[test]
fn fingerprints_hold_when_lines_move_and_duplicates_merge_errors_first() {
    let a = item("error", "src/a.rs", 10, "expected 3 args", "    f(1, 2);", "rustc");
    assert_eq!(fingerprint(&a), fingerprint(&item("error", "src/a.rs", 14, "expected 4 args", "f(1, 2);  ", "rustc")));
    assert_ne!(fingerprint(&a), fingerprint(&item("error", "src/b.rs", 10, "expected 3 args", "    f(1, 2);", "rustc")));
    let items = vec![
        item("warning", "src/a.rs", 2, "unused", "let x = 1;", "rustc"),
        item("error", "src/b.rs", 1, "bad", "b", "rustc"),
        item("hint", "src/a.rs", 1, "style", "a", "rustc"),
        a.clone(),
        item("error", "src/a.rs", 10, "expected 3 args", "    f(1, 2);", "rust-analyzer"),
    ];
    let merged: Vec<(String, String, Vec<String>)> = merge(items).into_iter().map(|i| (i.id, i.loc.unwrap().path, i.sources)).collect();
    let want = [("E1", "src/a.rs", vec!["rustc", "rust-analyzer"]), ("E2", "src/b.rs", vec!["rustc"]), ("W1", "src/a.rs", vec!["rustc"]), ("H1", "src/a.rs", vec!["rustc"])];
    assert_eq!(merged, want.map(|(id, p, s)| (id.to_string(), p.to_string(), s.into_iter().map(String::from).collect::<Vec<_>>())));
}

const GRADLE_REPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE testsuite>
<testsuite name="com.acme.data.OrderRepositoryTest" tests="4" skipped="1" failures="1" errors="1">
  <!-- written by Gradle; <testcase> in a comment is not a test -->
  <properties/>
  <testcase name="saves order with lines" classname="com.acme.data.OrderRepositoryTest" time="0.2">
    <failure message="org.opentest4j.AssertionFailedError: expected: &lt;3&gt; but was: &lt;2&gt;" type="org.opentest4j.AssertionFailedError"><![CDATA[org.opentest4j.AssertionFailedError: expected: <3> but was: <2>
	at org.junit.jupiter.api.AssertionUtils.fail(AssertionUtils.java:151)
	at com.acme.data.OrderRepositoryTest.saves order with lines(OrderRepositoryTest.kt:42)
]]></failure>
    <system-out><![CDATA[lots of <output> that is never kept]]></system-out>
  </testcase>
  <testcase name="finds &quot;by&quot; region &#x2014; &#38; more" classname="com.acme.data.ShipmentQueryTest" time="0.1">
    <error message="boom &amp; more" type="java.lang.IllegalStateException"/>
  </testcase>
  <testcase name="is skipped" classname="com.acme.data.OrderRepositoryTest"><skipped/></testcase>
  <testcase name="passes" classname="com.acme.data.OrderRepositoryTest" time="0.0"/>
  <system-err><![CDATA[]]></system-err>
</testsuite>
"#;

#[test]
fn gradle_report_with_cdata_entities_and_skips_parses() {
    let cases = junit::scan(GRADLE_REPORT.as_bytes()).unwrap();
    let got: Vec<(&str, &str, &str)> = cases.iter().map(|c| (c.class.as_str(), c.name.as_str(), c.status)).collect();
    assert_eq!(
        got,
        [
            ("com.acme.data.OrderRepositoryTest", "saves order with lines", "failed"),
            ("com.acme.data.ShipmentQueryTest", "finds \"by\" region \u{2014} & more", "failed"),
            ("com.acme.data.OrderRepositoryTest", "is skipped", "skipped"),
            ("com.acme.data.OrderRepositoryTest", "passes", "passed"),
        ]
    );
    assert!(cases[0].text.starts_with("org.opentest4j.AssertionFailedError: expected: <3> but was: <2>\norg.opentest4j"), "{:?}", cases[0].text);
    assert!(cases[0].text.contains("(OrderRepositoryTest.kt:42)") && !cases[0].text.contains("never kept"));
    assert_eq!(cases[1].text, "boom & more\n");
    assert_eq!(junit::decode("a &lt;b&gt; &#65;&#x42; &unknown; & c"), "a <b> AB &unknown; & c");
}

/// The gradle extension's `gradle:test`, with its report glob pointed at `glob`.
fn gradle_test() -> Action {
    let ext = parse_ext(BUILT_INS.iter().find(|b| b.0 == "gradle").unwrap().1, "gradle").unwrap();
    ext.actions.into_iter().find(|a| a.label == "gradle:test").unwrap()
}

fn write_report(root: &Path, path: &str, body: &str, age: Duration) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() - age).unwrap();
}

#[test]
fn reports_older_than_the_run_are_marked_stale() {
    let tmp = TempDir::new("itse-parse");
    write_report(&tmp.0, "data/build/test-results/test/TEST-A.xml", GRADLE_REPORT, Duration::from_secs(600));
    let started = SystemTime::now() - Duration::from_secs(60);
    let mut out = Parsed::default();
    junit::reports(&tmp.0, "**/build/test-results/**/TEST-*.xml", started, &gradle_test(), &mut trace::Index::new(&tmp.0), &mut out).unwrap();
    assert_eq!((out.counts, out.notes.clone()), (Some([1, 2, 1]), vec!["reports: 1 files, report written before this run".to_string()]));
    write_report(&tmp.0, "web/build/test-results/test/TEST-B.xml", r#"<testsuite><testcase name="t" classname="B"/></testsuite>"#, Duration::ZERO);
    let mut out = Parsed::default();
    junit::reports(&tmp.0, "**/build/test-results/**/TEST-*.xml", started, &gradle_test(), &mut trace::Index::new(&tmp.0), &mut out).unwrap();
    assert_eq!((out.counts, out.notes), (Some([1, 0, 0]), vec!["reports: 1 files written by this run".to_string()]), "only the fresh report is read");
}

#[test]
fn a_malformed_report_does_not_hide_the_others() {
    let tmp = TempDir::new("itse-parse");
    write_report(&tmp.0, "a/build/test-results/test/TEST-A.xml", GRADLE_REPORT, Duration::ZERO);
    write_report(&tmp.0, "b/build/test-results/test/TEST-X.xml", &GRADLE_REPORT[..900], Duration::ZERO);
    write_report(&tmp.0, "c/build/test-results/test/TEST-Y.xml", "<testsuite><testcase name='x'></testsuite>", Duration::ZERO);
    let mut out = Parsed::default();
    junit::reports(&tmp.0, "**/build/test-results/**/TEST-*.xml", SystemTime::now() - Duration::from_secs(5), &gradle_test(), &mut trace::Index::new(&tmp.0), &mut out).unwrap();
    let errors: Vec<&str> = out.items.iter().filter(|i| i.kind == "error").map(|i| i.msg.as_str()).collect();
    assert_eq!(errors, ["unparsed report b/build/test-results/test/TEST-X.xml at byte 901", "unparsed report c/build/test-results/test/TEST-Y.xml at byte 42"]);
    assert_eq!(out.counts, Some([1, 2, 1]), "the good report still counts");
    assert_eq!(out.items.iter().filter(|i| i.kind == "failed").count(), 2);
}

#[test]
fn framework_frames_collapse_to_a_count() {
    let tmp = TempDir::new("itse-parse");
    let src = |p: &str, body: &str| {
        fs::create_dir_all(tmp.0.join(p).parent().unwrap()).unwrap();
        fs::write(tmp.0.join(p), body).unwrap();
    };
    src("data/src/test/kotlin/com/acme/data/OrderRepositoryTest.kt", &("// test\n".repeat(41) + "        assertEquals(3, saved.lines.size)\n"));
    src("data/src/main/kotlin/com/acme/data/OrderRepository.kt", &("// main\n".repeat(87) + "        lines.dropLast(1).forEach(::insert)\n"));
    src("other/src/main/kotlin/OrderRepository.kt", "// a same-named file elsewhere\n");
    let trace = "org.opentest4j.AssertionFailedError: expected: <3> but was: <2>
\tat org.junit.jupiter.api.AssertionUtils.fail(AssertionUtils.java:151)
\tat org.junit.jupiter.api.AssertEquals.assertEquals(AssertEquals.java:150)
\tat com.acme.data.OrderRepository.save(OrderRepository.kt:88)
\tat com.acme.data.OrderRepositoryTest.saves order with lines(OrderRepositoryTest.kt:42)
\tat java.lang.reflect.Method.invoke(Method.java:580)
\tat org.junit.platform.commons.util.ReflectionUtils.invokeMethod(ReflectionUtils.java:728)
\tat org.gradle.api.internal.tasks.testing.Worker.run(Worker.java:60)
Caused by: java.io.IOException: Could not find a valid Docker environment
\tat org.testcontainers.DockerClientFactory.client(DockerClientFactory.java:200)
\tat java.lang.Thread.run(Thread.java:1583)
\t... 61 more
";
    let a = gradle_test();
    let (detail, loc, cut) = trace::trim(trace, "com.acme.data.OrderRepositoryTest", a.frames.as_ref(), &a.classify, &mut trace::Index::new(&tmp.0));
    assert_eq!(
        detail,
        [
            "org.opentest4j.AssertionFailedError: expected: <3> but was: <2>",
            "at org.junit.jupiter.api.AssertionUtils.fail  AssertionUtils.java:151",
            "at com.acme.data.OrderRepository.save  data/src/main/kotlin/com/acme/data/OrderRepository.kt:88",
            "     88 |         lines.dropLast(1).forEach(::insert)",
            "at com.acme.data.OrderRepositoryTest.saves order with lines  data/src/test/kotlin/com/acme/data/OrderRepositoryTest.kt:42",
            "     42 |         assertEquals(3, saved.lines.size)",
            "Caused by: java.io.IOException: Could not find a valid Docker environment",
            "at org.testcontainers.DockerClientFactory.client  DockerClientFactory.java:200",
            "… 5 framework frames",
            "class: environment (docker unavailable)",
        ]
    );
    let loc = loc.unwrap();
    assert_eq!((loc.path.as_str(), loc.line, cut), ("data/src/test/kotlin/com/acme/data/OrderRepositoryTest.kt", 42, false), "the test's own frame locates it");
}

#[test]
fn the_file_index_stops_at_its_cap() {
    let tmp = TempDir::new("itse-parse");
    for d in ["a", "build", "target", "node_modules", ".git"] {
        fs::create_dir_all(tmp.0.join(d)).unwrap();
        fs::write(tmp.0.join(d).join("F.kt"), "").unwrap();
    }
    let (names, cut) = trace::walk(&tmp.0, &[".git", "build", "target", "node_modules"], 10);
    assert_eq!((names, cut), (vec!["a/F.kt".to_string()], false));
    assert!(trace::walk(&tmp.0, &[], 3).1);
}

#[test]
fn baselines_are_kept_per_key() {
    let text = r#"{"hook:9f3a1c7e5d2b4410": {"fingerprints": {"ab": ["error", "src/a.rs"]}, "at": "t"}, "cli:cli": {"fingerprints": {}, "at": "t"}}"#;
    let baselines: BTreeMap<String, Baseline> = serde_json::from_str(text).unwrap();
    assert_eq!(baselines["hook:9f3a1c7e5d2b4410"].fingerprints["ab"], ["error", "src/a.rs"]);
}

#[test]
fn alike_problems_on_different_lines_stay_apart() {
    let items = vec![
        item("warning", "src/lib.rs", 11, "unused variable: `unused`", "let unused = 1;", "rustc"),
        item("warning", "src/lib.rs", 12, "unused variable: `unused`", "let unused = 1;", "rustc"),
    ];
    let merged: Vec<(String, u32)> = merge(items).into_iter().map(|i| (i.id, i.loc.unwrap().line)).collect();
    assert_eq!(merged, [("W1".to_string(), 11), ("W2".to_string(), 12)]);
}

#[test]
fn a_self_closing_tag_longer_than_the_tag_cap_still_closes() {
    let long = "x".repeat(20_000);
    let doc = format!(r#"<testsuite><testcase name="t" classname="C"><failure message="{long}" type="X"/></testcase><testcase name="u" classname="C"/></testsuite>"#);
    let cases = junit::scan(doc.as_bytes()).unwrap();
    assert_eq!(cases.iter().map(|c| (c.name.as_str(), c.status)).collect::<Vec<_>>(), [("t", "failed"), ("u", "passed")]);
}

#[test]
fn only_the_newest_stale_reports_are_read() {
    let tmp = TempDir::new("itse-parse");
    write_report(&tmp.0, "data/build/test-results/test/TEST-A.xml", GRADLE_REPORT, Duration::from_secs(60));
    write_report(&tmp.0, "data/build/test-results/test/TEST-B.xml", r#"<testsuite><testcase name="t" classname="B"/></testsuite>"#, Duration::from_secs(70));
    write_report(&tmp.0, "web/build/test-results/test/TEST-C.xml", GRADLE_REPORT, Duration::from_secs(3600));
    let mut out = Parsed::default();
    junit::reports(&tmp.0, "**/build/test-results/**/TEST-*.xml", SystemTime::now(), &gradle_test(), &mut trace::Index::new(&tmp.0), &mut out).unwrap();
    assert_eq!((out.counts, out.notes), (Some([2, 2, 1]), vec!["reports: 2 files, report written before this run".to_string()]), "web's older run is left out");
}

/// A state dir whose `runs/N` hold ended runs of `label` with `items`; a run without items has no
/// result yet (still being parsed).
fn runs(state: &Path, list: &[(u64, &str, Option<Vec<Item>>)]) -> Store {
    let store = Store(state.to_path_buf());
    for (n, label, items) in list {
        let dir = state.join(format!("runs/{n}"));
        fs::create_dir_all(&dir).unwrap();
        let exit = crate::actions::RunExit { code: Some(1), ended_at: "2026-11-05T10:00:00.000Z".into(), ..Default::default() };
        let rec = RunRecord { spec: crate::actions::RunSpec { label: label.to_string(), ..Default::default() }, exit: Some(exit), ..RunRecord::default() };
        fs::write(dir.join("run.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
        if let Some(items) = items {
            store.save(&Outcome { id: Some(format!("run:{n}")), items: items.clone(), ..Outcome::new("run", Verdict::Fail, *label, "") }, "").unwrap();
        }
    }
    store
}

#[test]
fn a_scoped_new_call_replaces_only_the_baseline_it_shows() {
    let tmp = TempDir::new("itse-parse");
    let (a, t) = (item("error", "src/a.rs", 3, "bad", "a", "rustc"), item("error", "tests/t.rs", 4, "worse", "t", "rustc"));
    let store = runs(&tmp.0, &[(1, "cargo:build", Some(vec![a.clone(), t.clone()]))]);
    assert_eq!(problems(&store, "", false, true, "cli:cli", &mut settled()).unwrap().counts, "2 new errors, 0 new warnings; 0 unchanged, 0 fixed");
    runs(&tmp.0, &[(2, "cargo:build", Some(vec![a, item("error", "tests/t.rs", 4, "worst", "t", "rustc")]))]);
    let scoped = problems(&store, "tests/", false, true, "cli:cli", &mut settled()).unwrap();
    assert!(scoped.counts.starts_with("1 new error, 0 new warnings; 0 unchanged, 1 fixed since "), "{}", scoped.counts);
    let whole = problems(&store, "", false, true, "cli:cli", &mut settled()).unwrap();
    assert!(whole.counts.starts_with("0 new errors, 0 new warnings; 2 unchanged, 0 fixed since "), "src/a.rs stayed in the baseline: {}", whole.counts);
}

#[test]
fn an_unsettled_call_does_not_advance_the_baseline() {
    let tmp = TempDir::new("itse-parse");
    let store = runs(&tmp.0, &[(1, "cargo:build", Some(vec![])), (2, "cargo:build", None)]);
    let o = problems(&store, "", false, true, "cli:cli", &mut settled()).unwrap();
    assert_eq!(o.notes, ["run:2 of cargo:build is still being parsed", "from cargo:build run:1, ended 2026-11-05T10:00:00.000Z", "not settled: the baseline is not advanced"]);
    assert!(!tmp.0.join("baselines.json").exists());
    runs(&tmp.0, &[(2, "cargo:build", Some(vec![item("error", "src/a.rs", 3, "bad", "a", "rustc")]))]);
    let o = problems(&store, "", false, true, "cli:cli", &mut settled()).unwrap();
    assert_eq!((o.counts.as_str(), o.items.len()), ("1 new error, 0 new warnings; 0 unchanged, 0 fixed", 1), "the parsed run's error is new once settled");
}

#[test]
fn a_compiler_and_a_language_server_reporting_one_error_merge() {
    let item = |msg: &str, detail: &[&str], source: &str| Item {
        kind: "error".into(),
        loc: Some(Loc { path: "src/lib.rs".into(), line: 6, col: 7 }),
        src: vec![(6, "    a + \"b\"".into())],
        msg: msg.into(),
        detail: detail.iter().map(|d| d.to_string()).collect(),
        sources: vec![source.into()],
        ..Item::default()
    };
    // rustc's short form appends the span's label; rust-analyzer puts notes on the lines after.
    let ra = item("E0277: cannot add `&str` to `u32`", &["no implementation for `u32 + &str`", "the trait `Add<&str>` is not implemented for `u32`"], "rust-analyzer");
    let rustc = item("E0277: cannot add `&str` to `u32`: no implementation for `u32 + &str`", &[], "rustc");
    assert_eq!(fingerprint(&ra), fingerprint(&rustc));
    let merged = merge(vec![ra, rustc]);
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(merged[0].detail, ["the trait `Add<&str>` is not implemented for `u32`"], "the label is said once");
    assert_eq!(
        (merged[0].msg.as_str(), merged[0].sources.clone()),
        ("E0277: cannot add `&str` to `u32`: no implementation for `u32 + &str`", vec!["rust-analyzer".to_string(), "rustc".into()])
    );
    let unused = |name: &str| fingerprint(&item(&format!("unused_variables: unused variable: `{name}`"), &[], "rust-analyzer"));
    assert_ne!(unused("x"), unused("y"), "a quoted name is part of the message, not a label");
    let other = item("E0308: mismatched types", &[], "rust-analyzer");
    assert_ne!(fingerprint(&other), fingerprint(&merged[0]), "another error at the same place stays apart");
    let mut settled = settled();
    settled.items = vec![other];
    let tmp = TempDir::new("itse-parse");
    let o = problems(&Store(tmp.0.clone()), "", false, false, "cli:cli", &mut settled).unwrap();
    assert_eq!(o.counts, "1 error, 0 warnings", "pushed items are counted with the actions'");
}
