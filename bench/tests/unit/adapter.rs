use std::path::PathBuf;

use serde_json::json;

use super::*;

fn vars(pairs: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
    pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

fn pointers() -> BTreeMap<String, String> {
    [("cost_usd", "/total_cost_usd"), ("turns", "/num_turns"), ("fresh_tokens", "/usage/input_tokens"), ("cache_write_tokens", "/usage/cache_creation_input_tokens")]
        .iter()
        .map(|(f, p)| (f.to_string(), p.to_string()))
        .collect()
}

#[test]
fn placeholders_are_filled_once_and_json_braces_stay() {
    let v = vars(&[("ws", "/t/ws"), ("mcp_config", "{\"mcpServers\":{}}"), ("arm", "{ws}")]);
    assert_eq!(template("--dir={ws}/x", &v).unwrap(), "--dir=/t/ws/x");
    assert_eq!(template("{mcp_config}", &v).unwrap(), "{\"mcpServers\":{}}");
    assert_eq!(template("{arm}", &v).unwrap(), "{ws}", "a value is not templated again");
    assert_eq!(template("{\"a\":{}} {} {", &v).unwrap(), "{\"a\":{}} {} {");
    assert_eq!(template("{nope}", &v).unwrap_err().to_string(), "unknown placeholder {nope} in '{nope}'");
}

#[test]
fn json_last_reads_the_last_json_object_line_and_misses_are_null() {
    let out = "starting\n{\"num_turns\": 1}\n[1, 2]\n  {\"total_cost_usd\": 0.25, \"num_turns\": 7, \"usage\": {\"input_tokens\": 1200}}  \nbye\n";
    let got = fields(Parser::JsonLast, out, &pointers());
    assert_eq!(
        Value::Object(got),
        json!({"cost_usd": 0.25, "turns": 7, "fresh_tokens": 1200, "cache_write_tokens": null, "cache_read_tokens": null, "output_tokens": null, "is_error": null})
    );
}

#[test]
fn no_json_line_or_parser_none_gives_every_field_as_null() {
    let nulls = json!({"cost_usd": null, "turns": null, "fresh_tokens": null, "cache_write_tokens": null, "cache_read_tokens": null, "output_tokens": null, "is_error": null});
    assert_eq!(Value::Object(fields(Parser::JsonLast, "no json here\n{broken\n", &pointers())), nulls);
    assert_eq!(Value::Object(fields(Parser::None, "{\"num_turns\": 3}\n", &pointers())), nulls);
}

#[test]
fn the_shipped_adapters_load_and_bad_ones_are_refused() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let shipped = load(&dir.join("adapters.toml")).unwrap();
    let claude = &shipped["claude-code"];
    assert_eq!((claude.parser, claude.command[0].as_str()), (Parser::JsonLast, "claude"));
    assert!(claude.command.iter().any(|a| a == "--max-turns") && !claude.command.iter().any(|a| a.contains("budget")), "turns capped, no dollar cap");
    assert_eq!(claude.pass_env, ["ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN"]);
    assert_eq!(claude.require_env, ["CLAUDE_CODE_OAUTH_TOKEN"], "a fresh config has no login, so the token must be there");
    assert_eq!((claude.env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"].as_str(), claude.env["DISABLE_AUTOUPDATER"].as_str()), ("1", "1"), "quiet whatever the env holds");
    let with: Value = serde_json::from_str(&claude.arms["with"]["mcp_config"]).unwrap();
    assert_eq!(with, json!({"mcpServers": {"itse": {"type": "stdio", "command": "itse", "args": ["mcp"]}}}));
    assert!(load(&dir.join("tests/fixtures/adapters.toml")).unwrap().contains_key("fake"));

    let tmp = std::env::temp_dir().join(format!("itse-bench-adapters-{}.toml", std::process::id()));
    let err = |text: &str| {
        fs::write(&tmp, text).unwrap();
        format!("{:#}", load(&tmp).unwrap_err()).replace(&tmp.display().to_string(), "A")
    };
    let base = "[h]\nversion_cmd = [\"h\"]\ncommand = [\"h\"]\nparser = \"json-last\"\n[h.arms.with]\n";
    assert_eq!(err(base), "A: [h.arms] must hold [h.arms.with] and [h.arms.without], not [\"with\"]");
    assert!(err(&format!("{base}[h.arms.without]\n[h.fields]\ntokens = '/usage'\n")).starts_with("A: [h.fields] tokens = '/usage'"));
    assert!(err(&format!("{base}[h.arms.without]\n[h.fields]\nturns = 'num_turns'\n")).starts_with("A: [h.fields] turns = 'num_turns'"));
    assert!(err(&base.replace("json-last", "xml")).contains("unknown variant `xml`"));
    assert!(err("[h]\ncommand = [\"h\"\n").starts_with("A: 3:1: expected ',' or ']'"), "{}", err("[h]\ncommand = [\"h\"\n"));
    let _ = fs::remove_file(&tmp);
}

#[test]
fn claude_codes_config_dir_is_where_integrate_puts_its_skill() {
    // The bench runs `itse integrate claude-code --skill --home {cfg}`, which puts the manifest's `~` at {cfg}.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let claude = &load(&dir.join("adapters.toml")).unwrap()["claude-code"];
    let manifest: Value = serde_json::from_slice(&fs::read(dir.join("../ext/claude-code/itse-ext.json")).unwrap()).unwrap();
    let skill = manifest["integration"]["skill"]["path"].as_str().unwrap().replacen('~', "/t/cfg", 1);
    let config = template(&claude.env["CLAUDE_CONFIG_DIR"], &vars(&[("cfg", "/t/cfg")])).unwrap();
    assert_eq!(skill.strip_prefix(&format!("{config}/")), Some("skills/itse/SKILL.md"), "{config} must hold {skill}");
    let fake = &load(&dir.join("tests/fixtures/adapters.toml")).unwrap()["fake"];
    assert_eq!(fake.env["CLAUDE_CONFIG_DIR"], claude.env["CLAUDE_CONFIG_DIR"]);
}
