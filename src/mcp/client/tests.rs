use super::*;

/// `n` tools as a server lists them, each with a description of a few sentences.
fn served(n: usize) -> Vec<Value> {
    let about = |i| format!("Tool {i} searches the index. It takes a query and returns matches, ranked, with {}.", "more words ".repeat(10));
    (1..=n).map(|i| json!({"name": format!("tool-{i}"), "description": about(i), "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}})).collect()
}

fn names(l: &Listing) -> Vec<String> {
    l.tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn exposed_tools_are_namespaced_and_names_past_64_chars_skipped() {
    let mut l = Listing::new(crate::mcp::tools(false));
    let long = "x".repeat(MAX_NAME - "some-mcp__".len() + 1);
    let tools = [served(2), vec![json!({"name": long, "description": "Too long.", "inputSchema": {"type": "object"}})]].concat();
    l.expose("some-mcp", None, SERVER_BYTES, &tools);
    assert_eq!(names(&l)[3..], ["some_mcp__tool_1", "some_mcp__tool_2"], "after the core tools, '-' as '_'");
    assert_eq!(l.routes["some_mcp__tool_2"], ("some-mcp".to_string(), "tool-2".to_string()), "a call goes to the server's own name");
    assert_eq!(l.tools[3]["inputSchema"], tools[0]["inputSchema"], "schemas pass unchanged");
    assert_eq!(l.notes, ["some-mcp: 1 tool skipped, named over 64 chars as some-mcp__<tool>"]);
    let exact = "y".repeat(MAX_NAME - "some_mcp__".len());
    l.expose("some-mcp", None, SERVER_BYTES, &[json!({"name": exact, "description": "Fits."})]);
    assert_eq!(l.tools.last().unwrap()["name"].as_str().unwrap().len(), 64, "64 chars fit");
    // Claude's names match ^[a-zA-Z0-9_-]{1,64}$, and '-' is '_' already.
    let odd = ["files.read", "ns/get", "sp ace", "ünï"].map(|n| json!({"name": n, "description": "Odd.", "inputSchema": {"type": "object"}}));
    let before = l.tools.len();
    l.expose("srv", None, SERVER_BYTES, &[odd.to_vec(), vec![json!({"name": "Get2", "description": "Fine."})]].concat());
    assert_eq!(names(&l)[before..], ["srv__Get2"]);
    assert_eq!(l.notes[1], "srv: 4 tools skipped, named with characters MCP clients refuse");
}

#[test]
fn an_exposed_tool_keeps_its_boolean_hints_and_drops_its_title() {
    let mut l = Listing::new(crate::mcp::tools(false));
    let annotations = json!({"title": "Search", "readOnlyHint": true, "destructiveHint": false, "openWorldHint": "yes", "x": 1});
    let tool = json!({"name": "search", "title": "Search notes", "description": "Searches.", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object"}, "annotations": annotations});
    l.expose("srv", None, SERVER_BYTES, &[tool, json!({"name": "plain", "description": "Plain."})]);
    assert_eq!(
        l.tools[3],
        json!({"name": "srv__search", "description": "Searches.", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true, "destructiveHint": false}})
    );
    assert!(l.tools[4].get("annotations").is_none(), "none without hints");
    let list = json!({"tools": l.tools}).to_string().len() + text("instructions", &[]).len();
    assert_eq!(l.bytes, list, "the hints count against the cap");
}

#[test]
fn a_description_is_clipped_to_its_first_sentence() {
    assert_eq!(about("Searches the index. Then ranks."), "Searches the index.");
    assert_eq!(about("  Search files matching a pattern in the\n    workspace. Returns paths."), "Search files matching a pattern in the workspace.", "a wrapped sentence");
    assert_eq!(about("\nEcho a text\n\nArgs: text"), "Echo a text", "the first paragraph only");
    assert_eq!(about("Is it up? Then ping."), "Is it up?");
    assert_eq!(about("Ends with no space."), "Ends with no space.");
    assert_eq!(about(&"word ".repeat(50)).chars().count(), MAX_ABOUT);
    assert_eq!(about("ünïcödé ".repeat(40).as_str()).chars().count(), MAX_ABOUT, "cut on chars, not bytes");
}

#[test]
fn two_tools_named_alike_once_dashes_are_underscores_are_listed_once() {
    let mut l = Listing::new(crate::mcp::tools(false));
    let tool = |name: &str| json!({"name": name, "description": "Reads a file.", "inputSchema": {"type": "object"}});
    l.expose("srv", None, SERVER_BYTES, &[tool("get-file"), tool("get_file"), tool("put")]);
    assert_eq!(names(&l)[3..], ["srv__get_file", "srv__put"], "no name twice in the list");
    assert_eq!(l.routes["srv__get_file"].1, "get-file", "the first keeps the name");
    assert_eq!(l.notes, ["srv: 1 tool skipped, named as another tool is once '-' is '_'"]);
    let list = json!({"tools": l.tools}).to_string().len() + text("instructions", &[]).len();
    assert_eq!(l.bytes, list, "a skipped tool counts nothing");
}

#[test]
fn exposed_tools_past_the_cap_are_dropped_and_named() {
    let mut l = Listing::new(crate::mcp::tools(true));
    l.expose("some-mcp", None, usize::MAX, &served(14));
    let kept = l.tools.len() - 4;
    assert!(kept > 0 && kept < 14, "{kept} kept");
    assert_eq!(names(&l)[4..], (1..=kept).map(|i| format!("some_mcp__tool_{i}")).collect::<Vec<_>>(), "dropped in server order");
    assert_eq!(l.notes, [format!("some-mcp: {} of 14 tools dropped (list cap 3,600 B); set tools: [...]", 14 - kept)]);
    let list = json!({"tools": l.tools}).to_string().len() + text("instructions", &[]).len();
    assert_eq!(l.bytes, list, "the count is the list as sent");
    assert!(l.bytes <= crate::mcp::LIST_CAP, "{} B", l.bytes);
    l.expose("other", None, usize::MAX, &served(1));
    assert_eq!(l.tools.len(), kept + 4, "a full list takes no more");
    assert_eq!(l.notes[1], "other: 1 of 1 tools dropped (list cap 3,600 B); set tools: [...]");
}

#[test]
fn the_allowlist_picks_tools_and_max_list_bytes_caps_a_server() {
    let mut l = Listing::new(crate::mcp::tools(false));
    let allow = ["tool-9".to_string(), "tool-2".to_string(), "absent".to_string()];
    l.expose("some-mcp", Some(&allow), SERVER_BYTES, &served(12));
    assert_eq!(names(&l)[3..], ["some_mcp__tool_2", "some_mcp__tool_9"], "only those named, in the server's order");
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let mut small = Listing::new(crate::mcp::tools(false));
    small.expose("some-mcp", None, 200, &served(12));
    assert_eq!(small.tools.len(), 3 + 1, "one tool fits 200 B");
    assert_eq!(small.notes, ["some-mcp: 11 of 12 tools dropped (max_list_bytes 200 B); set tools: [...]"]);
}
