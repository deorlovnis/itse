//! The flat TOML subset of task.toml and adapters.toml, as JSON: `[a.b]` headers, `key = value` with
//! "basic" (escapes `\" \\ \n \t \r`) and 'literal' strings, numbers, booleans, arrays of them over
//! lines, and `#` comments. Anything else is an error at its line:col, so a typo is no default.

use std::collections::BTreeSet;
use std::fmt::Display;

use anyhow::{Result, bail};
use serde_json::{Map, Number, Value};

pub fn parse(text: &str) -> Result<Value> {
    let mut p = Parser { s: text.chars().collect(), i: 0 };
    let mut root = Map::new();
    let (mut path, mut tables) = (vec![], BTreeSet::new());
    loop {
        p.gap();
        let at = p.i;
        match p.peek() {
            None => return Ok(Value::Object(root)),
            Some('[') => {
                path = p.header()?;
                if !tables.insert(path.clone()) {
                    return p.fail(at, format!("table [{}] is defined twice", path.join(".")));
                }
                table(&mut root, &path).or_else(|e| p.fail(at, e))?;
            }
            Some(_) => {
                let key = p.key()?;
                p.blank();
                if p.bump() != Some('=') {
                    return p.fail(p.i - 1, "expected '=' after the key");
                }
                p.blank();
                let value = p.value()?;
                p.eol()?;
                let t = table(&mut root, &path).or_else(|e| p.fail(at, e))?;
                if t.insert(key.clone(), value).is_some() {
                    return p.fail(at, format!("key '{key}' is defined twice"));
                }
            }
        }
    }
}

/// The table at `path`, made on the way; a key that holds a value there is an error.
fn table<'a>(root: &'a mut Map<String, Value>, path: &[String]) -> Result<&'a mut Map<String, Value>, String> {
    path.iter().try_fold(root, |t, key| match t.entry(key.clone()).or_insert_with(|| Value::Object(Map::new())) {
        Value::Object(inner) => Ok(inner),
        _ => Err(format!("'{key}' is a value, not a table")),
    })
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        self.i += 1;
        c
    }

    /// An error at char `at`, as `line:col: msg`, both 1-based.
    fn fail<T>(&self, at: usize, msg: impl Display) -> Result<T> {
        let before = &self.s[..at.min(self.s.len())];
        let line = before.iter().filter(|&&c| c == '\n').count() + 1;
        let col = before.len() - before.iter().rposition(|&c| c == '\n').map_or(0, |n| n + 1) + 1;
        bail!("{line}:{col}: {msg}")
    }

    /// Step over chars while `f` holds; where it started.
    fn skip(&mut self, f: impl Fn(char) -> bool) -> usize {
        let start = self.i;
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
        start
    }

    /// Spaces and tabs, and a comment up to the line's end.
    fn blank(&mut self) {
        self.skip(|c| c == ' ' || c == '\t');
        if self.peek() == Some('#') {
            self.skip(|c| c != '\n');
        }
    }

    /// Blanks, comments and line ends: what may stand between lines and inside arrays.
    fn gap(&mut self) {
        self.blank();
        while matches!(self.peek(), Some('\n' | '\r')) {
            self.i += 1;
            self.blank();
        }
    }

    fn eol(&mut self) -> Result<()> {
        self.blank();
        match (self.peek(), self.s.get(self.i + 1)) {
            (None | Some('\n'), _) | (Some('\r'), Some('\n')) => Ok(()),
            _ => self.fail(self.i, "expected the end of the line"),
        }
    }

    fn key(&mut self) -> Result<String> {
        let start = self.skip(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if start == self.i { self.fail(start, "expected a key") } else { Ok(self.s[start..self.i].iter().collect()) }
    }

    fn header(&mut self) -> Result<Vec<String>> {
        self.i += 1;
        let mut path = vec![];
        loop {
            self.blank();
            path.push(self.key()?);
            self.blank();
            match self.bump() {
                Some('.') => {}
                Some(']') => break,
                _ => return self.fail(self.i - 1, "expected '.' or ']' in the table header"),
            }
        }
        self.eol()?;
        Ok(path)
    }

    fn value(&mut self) -> Result<Value> {
        let at = self.i;
        match self.peek() {
            Some('"') => self.string('"'),
            Some('\'') => self.string('\''),
            Some('[') => {
                self.i += 1;
                let mut items = vec![];
                loop {
                    self.gap();
                    if self.peek() == Some(']') {
                        self.i += 1;
                        return Ok(Value::Array(items));
                    }
                    items.push(self.value()?);
                    self.gap();
                    match self.bump() {
                        Some(',') => {}
                        Some(']') => return Ok(Value::Array(items)),
                        _ => return self.fail(self.i - 1, "expected ',' or ']' in the array"),
                    }
                }
            }
            Some(c) if c.is_ascii_alphanumeric() || c == '-' || c == '+' => {
                self.skip(|c| c.is_ascii_alphanumeric() || "+-._".contains(c));
                let word: String = self.s[at..self.i].iter().filter(|&&c| c != '_').collect();
                let number = if word.contains(['.', 'e', 'E']) { word.parse().ok().and_then(Number::from_f64) } else { word.parse::<i64>().ok().map(Number::from) };
                match (word.as_str(), number) {
                    ("true", _) => Ok(Value::Bool(true)),
                    ("false", _) => Ok(Value::Bool(false)),
                    (_, Some(n)) => Ok(Value::Number(n)),
                    _ => self.fail(at, format!("'{word}' is not a string, number, boolean or array")),
                }
            }
            _ => self.fail(at, "expected a value"),
        }
    }

    /// A string on one line: `"` with escapes, or `'` taken literally.
    fn string(&mut self, quote: char) -> Result<Value> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return self.fail(start, "unterminated string"),
                Some(c) if c == quote => return Ok(Value::String(out)),
                Some('\\') if quote == '"' => match self.bump().and_then(|c| "\"\\ntr".find(c)) {
                    Some(n) => out.push(['"', '\\', '\n', '\t', '\r'][n]),
                    None => return self.fail(self.i - 2, "unknown escape"),
                },
                Some(c) => out.push(c),
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/toml.rs"]
mod tests;
