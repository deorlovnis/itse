use serde_json::json;

use super::*;

#[test]
fn tables_keys_and_every_value_kind_parse() {
    let text = r#"
version = "1.0"   # a comment
[metadata]
tags = ["rust", 'control',]
[a.b-c]
n = -3
big = 1_000
f = 0.5
e = 1e3
yes = true
no = false
s = "q\"\\\n\t"
lit = '{"mcpServers":{}}'
cmd = [
    "claude",  # inside an array
    "-p",
]
empty = []
"#;
    let doc = parse(text).unwrap();
    assert_eq!(
        doc,
        json!({"version": "1.0", "metadata": {"tags": ["rust", "control"]}, "a": {"b-c": {"n": -3, "big": 1000, "f": 0.5, "e": 1000.0, "yes": true, "no": false,
            "s": "q\"\\\n\t", "lit": "{\"mcpServers\":{}}", "cmd": ["claude", "-p"], "empty": []}}})
    );
}

#[test]
fn crlf_line_ends_and_an_empty_file_parse() {
    assert_eq!(parse("a = 1\r\n[t]\r\nb = 'x'\r\n").unwrap(), json!({"a": 1, "t": {"b": "x"}}));
    assert_eq!(parse("").unwrap(), json!({}));
    assert_eq!(parse("# only a comment").unwrap(), json!({}));
}

#[test]
fn errors_name_the_line_and_column() {
    let err = |text: &str| parse(text).unwrap_err().to_string();
    assert_eq!(err("a = 1\nb = nope\n"), "2:5: 'nope' is not a string, number, boolean or array");
    assert_eq!(err("a = 1\n  b 2"), "2:5: expected '=' after the key");
    assert_eq!(err("[t]\nx = \"open\n"), "2:5: unterminated string");
    assert_eq!(err("x = \"a\\qb\""), "1:7: unknown escape");
    assert_eq!(err("x = 1 2"), "1:7: expected the end of the line");
    assert_eq!(err("x = [1 2]"), "1:8: expected ',' or ']' in the array");
    assert_eq!(err("[a b]"), "1:4: expected '.' or ']' in the table header");
    assert_eq!(err("a.b = 1"), "1:2: expected '=' after the key");
    assert_eq!(err("x = { a = 1 }"), "1:5: expected a value");
    assert_eq!(err("= 1"), "1:1: expected a key");
}

#[test]
fn duplicates_and_values_used_as_tables_are_errors() {
    let err = |text: &str| parse(text).unwrap_err().to_string();
    assert_eq!(err("a = 1\na = 2"), "2:1: key 'a' is defined twice");
    assert_eq!(err("[t]\n[u]\n[t]"), "3:1: table [t] is defined twice");
    assert_eq!(err("a = 1\n[a.b]"), "2:1: 'a' is a value, not a table");
    assert_eq!(err("[t]\nx = 1\n[t.x]"), "3:1: 'x' is a value, not a table");
}
