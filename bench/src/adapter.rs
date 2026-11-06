//! Harness adapters from adapters.toml. `{name}` in `command`, `version_cmd` and `env` is filled from
//! the trial and the arm's table (an unknown name is an error; a `{` that opens none, as in JSON,
//! stays). `json-last` reads the last JSON object line of stdout by JSON pointers; `none` reads none.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::toml;

/// The optional fields of a results line an adapter may fill.
pub const FIELDS: &[&str] = &["fresh_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens", "cost_usd", "turns", "is_error"];
pub const ARMS: &[&str] = &["with", "without"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adapter {
    pub version_cmd: Vec<String>,
    pub command: Vec<String>,
    pub parser: Parser,
    /// Set for the harness only, templated: a fresh config dir per trial, say.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Passed to the harness from the bench's env when set there; nothing else of its kind is.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Must be set in the bench's env, or the run refuses to start: a fresh config holds no login.
    #[serde(default)]
    pub require_env: Vec<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// Per arm, more `{name}` values: the MCP config, say.
    pub arms: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Parser {
    JsonLast,
    None,
}

pub fn load(path: &Path) -> Result<BTreeMap<String, Adapter>> {
    let name = path.display();
    let text = fs::read_to_string(path).with_context(|| format!("adapters {name}"))?;
    let all: BTreeMap<String, Adapter> = serde_json::from_value(toml::parse(&text).with_context(|| format!("{name}"))?).with_context(|| format!("{name}"))?;
    for (id, a) in &all {
        ensure!(!a.command.is_empty() && !a.version_cmd.is_empty(), "{name}: [{id}] needs a command and a version_cmd");
        let arms: Vec<&str> = a.arms.keys().map(String::as_str).collect();
        ensure!(arms == ARMS, "{name}: [{id}.arms] must hold [{id}.arms.with] and [{id}.arms.without], not {arms:?}");
        if let Some((field, ptr)) = a.fields.iter().find(|(f, p)| !FIELDS.contains(&f.as_str()) || !p.starts_with('/')) {
            bail!("{name}: [{id}.fields] {field} = '{ptr}': fields are {} and values JSON pointers", FIELDS.join(", "));
        }
    }
    Ok(all)
}

/// `{name}` replaced by its value in `vars`; values are not templated again.
pub fn template(s: &str, vars: &BTreeMap<&str, String>) -> Result<String> {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let name = after.find('}').map(|end| &after[..end]).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        let value = name.map(|n| vars.get(n).with_context(|| format!("unknown placeholder {{{n}}} in '{s}'"))).transpose()?;
        out += &rest[..open];
        out += value.map_or("{", String::as_str);
        rest = &after[name.map_or(0, |n| n.len() + 1)..];
    }
    Ok(out + rest)
}

/// Every field of `FIELDS`, from the harness's stdout: the declared pointer's value, else null.
pub fn fields(parser: Parser, stdout: &str, pointers: &BTreeMap<String, String>) -> Map<String, Value> {
    let last = match parser {
        Parser::JsonLast => stdout.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(Value::is_object)),
        Parser::None => None,
    };
    let get = |field: &str| last.as_ref().zip(pointers.get(field)).and_then(|(v, p)| v.pointer(p)).cloned().unwrap_or(Value::Null);
    FIELDS.iter().map(|f| (f.to_string(), get(f))).collect()
}

#[cfg(test)]
#[path = "../tests/unit/adapter.rs"]
mod tests;
