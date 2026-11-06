use std::ffi::OsString;

use regex_lite::Regex;
use serde_json::json;

use super::*;
use crate::sys::tests::TempDir;
use crate::verbs;

/// `n` items of four lines each (header, source line, two detail lines), stored as `q:1`.
fn many(n: usize) -> Outcome {
    let items = (1..=n)
        .map(|i| Item {
            id: format!("T{i}"),
            kind: "test".into(),
            loc: Some(Loc { path: "src/lib.rs".into(), line: i as u32, col: 5 }),
            src: vec![(i as u32, format!("\tassert_eq!(f({i}), {i});"))],
            msg: format!("tests::case_{i}"),
            detail: vec!["assertion `left == right` failed".into(), format!("  left: {i}")],
            sources: vec![],
        })
        .collect();
    Outcome { id: Some("q:1".into()), items, ..Outcome::new("test", Verdict::Fail, "itse", format!("{n} failed")) }
}

fn args(tokens: &[&str]) -> Args {
    verbs::parse_args(verbs::find("show").unwrap(), tokens.iter().map(OsString::from)).unwrap()
}

/// Every page of an outcome, as text.
fn pages(o: &mut Outcome, via: &str) -> Vec<String> {
    let mut out = vec![o.render(via, 1, VERB, false).unwrap()];
    for n in 2..=o.page.of {
        out.push(o.render(via, n, VERB, false).unwrap());
    }
    out
}

fn item_ids(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.split(' ').next().filter(|w| w.starts_with('T') && w[1..].parse::<u32>().is_ok()).map(String::from)).collect()
}

#[test]
fn forty_one_items_page_without_splitting_an_item() {
    let mut o = many(41);
    let pages = pages(&mut o, "cli");
    assert!(pages.len() > 1);
    let mut seen = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        assert!(page.lines().count() <= VERB.0 && page.len() <= VERB.1, "page {} is {} lines, {} chars", i + 1, page.lines().count(), page.len());
        assert!(page.starts_with("FAIL test itse — 41 failed; q:1\n"), "every page starts with the verdict line");
        let lines: Vec<&str> = page.lines().collect();
        for (at, line) in lines.iter().enumerate().filter(|(_, l)| l.starts_with('T')) {
            let n = &line[1..line.find(' ').unwrap()];
            assert_eq!(lines[at + 1], format!("{n:>7} |     assert_eq!(f({n}), {n});"), "the source line follows its item, tabs as 4 spaces");
            assert_eq!(lines[at + 3], format!("     left: {n}"), "the item's last line is on its page");
        }
        seen.extend(item_ids(page));
    }
    assert_eq!(seen, (1..=41).map(|i| format!("T{i}")).collect::<Vec<_>>(), "each item once, in order");
    assert!(pages[0].ends_with(&format!("page 1 of {}; full: itse show q:1 --page 2\n", pages.len())));
    assert!(pages.last().unwrap().ends_with(&format!("page {0} of {0}\n", pages.len())));
    assert_eq!(o.render("cli", 9, VERB, false), Err(format!("page 9 of {}", pages.len())));
}

#[test]
fn json_pages_hold_the_same_items_as_text() {
    let mut o = many(41);
    for (i, text) in pages(&mut o, "cli").iter().enumerate() {
        let n = i as u32 + 1;
        let v: Value = serde_json::from_str(&o.render("cli", n, VERB, true).unwrap()).unwrap();
        assert_eq!(v["itse"], 1);
        assert_eq!(v["page"], json!({"n": n, "of": o.page.of}));
        let ids: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, item_ids(text), "page {n}");
    }
}

#[test]
fn templates_hold_no_imperatives() {
    let imperative = Regex::new(r"(?i)please|you should|you must|make sure|don't|do not|always|never").unwrap();
    for (key, template) in TEXT {
        assert!(!imperative.is_match(template), "{key}: {template}");
    }
    assert!(TEXT.iter().any(|(k, _)| *k == "instructions"), "the MCP instructions are a template too");
    assert_eq!(text("last", &[("n", &2), ("of", &3)]), "page 2 of 3");
}

#[test]
fn printed_pointers_are_fully_qualified() {
    let mut o = many(41);
    o.items[3].detail.extend((0..10).map(|i| format!("at frame {i}")));
    let pointer = Regex::new(r"itse show (\S+)").unwrap();
    let qualified = Regex::new(r"^(q|run):\d+(/[A-Z]\d+)?$").unwrap();
    let mut found = 0;
    for page in pages(&mut o, "cli").iter().chain(pages(&mut o, "mcp").iter()) {
        for c in pointer.captures_iter(page) {
            assert!(qualified.is_match(&c[1]), "{}", &c[0]);
            found += 1;
        }
    }
    assert!(found >= 4, "page and detail pointers were printed: {found}");
    assert!(pages(&mut o, "cli")[0].contains("   … 6 more lines: itse show q:1/T4\n"), "hidden detail is one show away");
}

#[test]
fn next_renders_as_a_tool_call_over_mcp() {
    let row = |next: &[&str], actor: &str| State {
        item: "kotlin-lsp".into(),
        state: "expiring".into(),
        actor: Some(actor.into()),
        next: Some(next.iter().map(|s| s.to_string()).collect()),
        ..State::default()
    };
    let mut o = Outcome { states: vec![row(&["lock", "--refresh", "kotlin-lsp"], "agent"), row(&["trust"], "human")], ..Outcome::new("status", Verdict::Unready, "", "") };
    let (cli, mcp) = (o.render("cli", 1, VERB, false).unwrap(), o.render("mcp", 1, VERB, false).unwrap());
    assert!(cli.contains("next (agent): itse lock --refresh kotlin-lsp\n"), "{cli}");
    assert!(mcp.contains(r#"next (agent): test_build_run {verb: "lock", args: ["--refresh", "kotlin-lsp"]}"#), "{mcp}");
    for text in [cli, mcp] {
        assert!(text.contains("next (human): itse trust in a terminal\n"), "a human's step is the CLI line for everyone: {text}");
    }
    assert_eq!(crate::mcp::next(&["show".into(), "run:3".into()], "mcp", false), "itse show run:3", "show_result takes no verb, so the pointer stays a CLI line");
}

#[test]
fn a_bare_item_id_resolves_from_the_cli() {
    let tmp = TempDir::new("itse-results");
    let store = Store(tmp.0.clone());
    let save = |id: &str, items: usize, session: &str| store.save(&Outcome { id: Some(id.into()), ..many(items) }, session).unwrap();
    save("q:1", 2, "cli");
    save("q:2", 1, "9f3a1c7e5d2b4410");
    save("q:3", 0, "cli");
    let show = |id: &str, session: &str| results_show(&store, &[id], session);
    let text = show("T1", "cli");
    assert!(text.starts_with("FAIL test itse — 2 failed; q:1\nT1 test src/lib.rs:1:5 tests::case_1\n"), "this session's newest result holding T1: {text}");
    assert!(text.ends_with("T1 from q:1, the newest result holding it\n"), "{text}");
    assert!(show("T1", "another").contains("T1 from q:2,"), "else the workspace's newest");
    assert_eq!(show("T7", "cli"), "ERROR show T7 — no stored result holds T7\n");
    assert_eq!(show("q:9", "cli"), "ERROR show q:9 — no stored result q:9\n");
    assert!(show("q:1/T2", "x").contains("T2 test src/lib.rs:2:5"), "a qualified item needs no session");
}

fn results_show(store: &Store, tokens: &[&str], session: &str) -> String {
    show(store, &args(tokens), "cli", session).text
}

#[test]
fn show_grep_and_tail() {
    let tmp = TempDir::new("itse-results");
    let store = Store(tmp.0.clone());
    store.save(&many(60), "cli").unwrap();
    let grep = results_show(&store, &["q:1", "--grep", r"case_5\d"], "cli");
    let lines: Vec<&str> = grep.lines().collect();
    assert_eq!(lines[0], "OK show — 10 of 241 lines match; q:1");
    assert_eq!(lines[1], "198: T50 test src/lib.rs:50:5 tests::case_50", "matches carry their line numbers");
    assert_eq!(lines.len(), 11);
    let tail = results_show(&store, &["q:1", "--tail", "2"], "cli");
    assert_eq!(tail, "OK show — last 2 of 241 lines; q:1\n   assertion `left == right` failed\n     left: 60\n");
    assert_eq!(results_show(&store, &["q:1", "--grep", "("], "cli").lines().next().unwrap().split(" — ").next(), Some("ERROR show q:1"));
    assert_eq!(results_show(&store, &["q:1", "--tail", "x"], "cli"), "ERROR show q:1 — --page and --tail take a number\n");
    let capped = results_show(&store, &["q:1", "--tail", "500"], "cli");
    assert_eq!(capped.lines().count(), SHOW.0 + 1, "the show cap holds for logs too");
    assert!(capped.ends_with("… 42 more lines: itse show q:1 --raw\n"), "{}", capped.lines().last().unwrap());
    assert_eq!(results_show(&store, &["q:1", "--raw"], "cli").lines().count(), 241, "--raw has no cap");
}

#[test]
fn the_full_log_is_one_show_away() {
    let tmp = TempDir::new("itse-results");
    let store = Store(tmp.0.clone());
    let mut o = many(41);
    o.items[0].detail.extend((0..9).map(|i| format!("at frame {i}")));
    let first = o.render("cli", 1, VERB, false).unwrap();
    store.save(&o, "cli").unwrap();
    let (mut seen, mut page) = (item_ids(&first), first);
    let pointer = Regex::new(r"full: itse show (\S+) --page (\d+)$").unwrap();
    while let Some(c) = pointer.captures(page.trim_end()) {
        page = results_show(&store, &[&c[1], "--page", &c[2]], "cli");
        seen.extend(item_ids(&page));
    }
    assert_eq!(seen.len(), 41, "following each printed pointer shows every item once");
    let detail = results_show(&store, &["q:1/T1"], "cli");
    assert!(detail.contains("   at frame 8\n") && !detail.contains("more lines"), "the item's whole detail: {detail}");
    assert!(results_show(&store, &["q:1", "--page", "9"], "cli").starts_with("ERROR show q:1 — page 9 of "));
}

#[test]
fn the_store_keeps_the_newest_200_results() {
    let tmp = TempDir::new("itse-results");
    let store = Store(tmp.0.clone());
    for n in 1..=205 {
        store.save(&Outcome { id: Some(format!("q:{n}")), ..many(1) }, "cli").unwrap();
    }
    assert_eq!(fs::read_dir(tmp.0.join("results")).unwrap().count(), 200);
    assert!(store.load("q:5").is_err() && store.load("q:6").is_ok());
    let v: Value = serde_json::from_slice(&fs::read(tmp.0.join("results/205.json")).unwrap()).unwrap();
    assert_eq!((v["itse"].clone(), v["session"].clone(), v["verdict"].clone()), (json!(1), json!("cli"), json!("fail")));
}

#[test]
fn the_verdict_line_states_cost_warmth_and_id() {
    let mut o = Outcome { cost: Cost { ms: 38_211, warm: Some(true) }, id: Some("run:14".into()), ..Outcome::new("test", Verdict::Fail, ":data", "2 failed, 141 passed") };
    assert_eq!(o.head(), "FAIL test :data — 2 failed, 141 passed; 38.2 s, warm; run:14");
    o.verdict = Verdict::NeedsHuman;
    (o.counts, o.cost, o.id) = (String::new(), Cost::default(), None);
    assert_eq!(o.head(), "NEEDS_HUMAN test :data");
    let long = Item { id: "E1".into(), src: vec![(3, "x".repeat(200))], ..Item::default() };
    let text = Outcome { items: vec![long], ..o }.render("cli", 1, VERB, false).unwrap();
    assert!(text.contains(&format!("      3 | {}…\n", "x".repeat(159))), "source lines are clipped at 160 chars");
}
