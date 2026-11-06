//! The manifest: extensions (one JSON file each; the built-ins compiled in from
//! `ext/<id>/itse-ext.json`), the workspace layers, detection, templates, and `itse use`/`unuse`.
//!
//! Every model refuses unknown keys, so a typo is an error that names the file instead of a
//! setting that silently does nothing. The one exception is an action: VS Code's UI keys are
//! accepted and ignored, so tasks.json entries paste verbatim (`Action::extra`).
//!
//! Layers: a detected extension is only a suggestion; the team layer (`.itse/itse.json` in the
//! worktree) is read, never written; the developer layer is one file per repo under
//! `~/.config/itse/workspaces/`, found by its `repo` field, so every worktree of a clone shares it.
//! Itse writes nothing under a workspace root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::host::{Limits, Protocol};
use crate::lock;
use crate::results::{Outcome, Verdict};
use crate::sys;
use crate::verbs::Args;
use crate::workspace::{Dirs, Workspace};

/// VS Code task keys about its UI: accepted in an action, and ignored.
pub const VS_CODE_UI_KEYS: [&str; 10] = ["presentation", "runOptions", "detail", "icon", "hide", "promptOnClose", "isBackground", "windows", "osx", "linux"];

/// One value or a list of them, as VS Code allows for `dependsOn`, `problemMatcher` and `pattern`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    // Many first: a derived struct also accepts a JSON array, field by field.
    Many(Vec<T>),
    One(T),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::Many(v) => v,
            OneOrMany::One(x) => std::slice::from_ref(x),
        }
    }
}

/// `itse-ext.json`: data only, at most 1K lines.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Extension {
    pub itse_ext: u32,
    pub id: String,
    pub version: Option<String>,
    pub requires: Vec<String>,
    /// Top-level file names whose presence suggests this extension.
    pub detect: Vec<String>,
    pub packages: BTreeMap<String, Package>,
    pub servers: Vec<Server>,
    pub actions: Vec<Action>,
    #[serde(rename = "problemMatchers")]
    pub problem_matchers: BTreeMap<String, Matcher>,
    pub skills: Vec<Skill>,
    /// Files in the repo that pin a version the project manages itself; read, never rewritten.
    pub project_pins: Vec<ProjectPin>,
    pub identity: Option<Identity>,
    /// Harness files and commands for `itse integrate`; only read, so kept as JSON.
    pub integration: Option<Value>,
    /// Hook recipes for `itse hook`; only read, so kept as JSON.
    pub hooks: Option<Value>,
    pub debug: Option<Debug>,
}

/// How `debug test` runs the tests under a debugger: the `dap` server it uses and the request it
/// sends (`launch`, so the adapter starts the tests), whose `arguments` take `${workspaceFolder}`,
/// `${input:target}` and `${input:filter}`. `debug attach <host:port>` sends `attach` with
/// `attach`. `project_packages` (class or module prefixes) scope the default stop on caught
/// exceptions to project code, through `caught_condition`, the adapter's own condition syntax for
/// its caught filter with `${packages}` (the prefixes, comma-separated); without one, every caught
/// exception stops and those outside the project are continued past. A test run that waits for a
/// debugger to attach (the JVM's) comes with the JVM transport; it will add the test action to run
/// (its `target_args` and `filter_args` reused) and the ready line whose `${ready:1}` is the port
/// that `attach` gets, so `debug test` attaches to `run:N` and its result says `test result in run:N`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Debug {
    pub adapter: String,
    pub request: Option<String>,
    pub arguments: Value,
    pub attach: Value,
    pub project_packages: Vec<String>,
    pub caught_condition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Package {
    pub purl: String,
    /// `fetch` (pinned, into the store) or `path` (found on PATH, unpinned).
    pub install: String,
    pub pin_source: Option<PinSource>,
    pub archive: Option<String>,
    pub strip: u32,
    pub bin: BTreeMap<String, String>,
    pub hosts: Vec<String>,
    pub expires_after_days: Option<u32>,
    /// Exit code → mapped reason, such as `"7": "server_expired"`.
    pub exit_codes: BTreeMap<String, String>,
    pub consents: Vec<ConsentSpec>,
    pub jvm_args: Vec<String>,
    pub jvm_args_env: Option<String>,
    pub executes_project_code: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PinSource {
    /// A GitHub release; each target's asset name, whose `digest` is the pin.
    GithubRelease { repo: String, assets: BTreeMap<String, String> },
    /// Open VSX `server-bundle.json` of an extension; target → bundle platform.
    OpenvsxBundle { extension: String, platforms: BTreeMap<String, String> },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsentSpec {
    pub id: String,
    /// `hash` (of a file in the package, such as an EULA) or `choice`.
    pub kind: String,
    pub of_file: Option<String>,
    pub hex_prefix: Option<usize>,
    pub flag: Option<String>,
    pub env: Option<String>,
    pub values: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub id: String,
    pub protocol: Protocol,
    pub transport: Option<String>,
    pub command: Vec<String>,
    pub filetypes: Vec<String>,
    pub languages: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    /// `initializationOptions`; absent, the `settings` under the server's id (rust-analyzer reads
    /// the same keys from both).
    pub init_options: Value,
    /// What `workspace/configuration` and `didChangeConfiguration` get, by section.
    pub settings: Value,
    pub adapter_id: Option<String>,
    pub via: Option<DapVia>,
    pub hide_vars: Vec<String>,
    /// `"client"` also sends `workspace/didChangeWatchedFiles`.
    pub watch: Option<String>,
    pub limits: Limits,
    pub idle_minutes: Option<u64>,
    pub timeout_s: Option<u64>,
    /// What the server itself writes into the workspace, like an action's `outputs`: rust-analyzer's
    /// `cargo metadata` makes a missing `Cargo.lock`.
    pub outputs: Vec<String>,
    pub executes_project_code: bool,
}

/// How a DAP adapter is reached.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DapVia {
    Program,
    Tcp {
        argv: Vec<String>,
        port: u16,
    },
    LspCommand {
        server: String,
        command: String,
        #[serde(default)]
        arguments: Vec<Value>,
    },
}

/// A VS Code task entry, plus Itse's additions from `target_args` on.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Action {
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
    pub args: Vec<String>,
    pub options: ActionOptions,
    #[serde(rename = "dependsOn")]
    pub depends_on: Option<OneOrMany<String>>,
    pub group: Option<Group>,
    #[serde(rename = "problemMatcher")]
    pub problem_matcher: Option<OneOrMany<MatcherRef>>,
    pub target_args: Vec<String>,
    pub filter_args: Vec<String>,
    /// Report parsers, such as `junit-xml:**/build/test-results/**/TEST-*.xml`.
    pub parse: Vec<String>,
    pub frames: Option<Frames>,
    pub classify: Vec<Classify>,
    pub counts: Option<Counts>,
    pub ready: Option<String>,
    pub drop_lines: Vec<String>,
    pub outputs: Vec<String>,
    /// Fixed notes every result of this action carries, such as what it leaves out.
    pub notes: Vec<String>,
    pub executes_project_code: bool,
    pub timeout_s: Option<u64>,
    /// Any other key; the loader refuses those outside `VS_CODE_UI_KEYS`.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionOptions {
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    /// VS Code's shell options: accepted so tasks.json entries paste, and ignored (an action runs
    /// its argv, never through a shell).
    pub shell: Option<Value>,
}

/// `"test"`, or `{kind, isDefault}`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Group {
    Kind(#[allow(dead_code)] String),
    Full {
        kind: String,
        #[serde(default, rename = "isDefault")]
        is_default: bool,
    },
}

/// `"$name"` of a named matcher, or one written in place.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MatcherRef {
    Name(String),
    Inline(Box<Matcher>),
}

/// The problemMatcher subset: single-line and fixed multi-line patterns, no `loop`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Matcher {
    pub owner: Option<String>,
    pub source: Option<String>,
    pub severity: Option<String>,
    /// `"absolute"`, `"relative"` or `["relative", dir]`.
    #[serde(rename = "fileLocation")]
    pub file_location: Option<OneOrMany<String>>,
    pub pattern: Option<OneOrMany<Pattern>>,
    /// Itse's addition: `"test"` makes T items instead of problems.
    pub kind: Option<String>,
    pub detail_lines: Option<usize>,
}

/// Capture group numbers for each field.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pattern {
    pub regexp: String,
    pub file: Option<usize>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub severity: Option<usize>,
    pub code: Option<usize>,
    pub message: Option<usize>,
    pub name: Option<usize>,
}

/// Stack frames in an action's output: which to keep, and which collapse to a count.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Frames {
    pub regexp: String,
    pub file: usize,
    pub line: usize,
    pub collapse: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Classify {
    pub regexp: String,
    pub class: String,
    pub note: Option<String>,
}

/// A summary line of a run's output, summed when it repeats (one per cargo test binary).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Counts {
    pub regexp: String,
    pub passed: Option<usize>,
    pub failed: Option<usize>,
    pub skipped: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub text: String,
}

/// A repo file read for status: JSON pointers for `.json`, keys for `.properties`, else the first line.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectPin {
    pub name: String,
    pub file: String,
    pub keys: Vec<String>,
}

impl ProjectPin {
    /// What status shows of the file: each key's value (`absent` if not set), or its first line;
    /// None without the file.
    pub fn read(&self, root: &Path) -> Option<String> {
        let text = fs::read_to_string(root.join(&self.file)).ok()?;
        if self.keys.is_empty() {
            return Some(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(60).collect());
        }
        let value = |key: &str| match self.file.ends_with(".json") {
            true => serde_json::from_str::<Value>(&text).ok().and_then(|v| v.pointer(key).map(|v| v.as_str().map_or(v.to_string(), String::from))),
            false => text.lines().find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().replace("\\:", ":"))),
        };
        Some(self.keys.iter().map(|k| format!("{k} {}", value(k).unwrap_or_else(|| "absent".into()))).collect::<Vec<_>>().join("; "))
    }
}

/// Env names and stdin fields a harness extension declares for caller identity.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    pub session_env: Option<String>,
    pub client_env: Option<String>,
    pub client_regexp: Option<String>,
    pub hook_session_from: Option<String>,
}

/// A workspace layer: the developer's (`~/.config/itse/workspaces/<name>.json`) or the team's
/// (`.itse/itse.json`, read only). The team layer has no `usage` key, nor any other.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layer {
    pub itse: u32,
    /// The repo key of the repo this developer layer is for.
    pub repo: Option<String>,
    /// Extension id → its `key=value` settings: strings, booleans or numbers.
    #[serde(rename = "use")]
    pub uses: BTreeMap<String, BTreeMap<String, Value>>,
    pub system: BTreeMap<String, SystemCheck>,
    pub servers: BTreeMap<String, ServerSettings>,
    pub inputs: Vec<Input>,
}

/// A typed check of something only the image provides; never a shell string.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemCheck {
    pub lib: Option<String>,
    pub cmd: Vec<String>,
}

/// The developer's settings for one server.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    pub idle_minutes: Option<u64>,
    pub settings: Option<Value>,
    pub expose: bool,
    pub tools: Option<Vec<String>>,
    pub max_list_bytes: Option<usize>,
}

/// A VS Code `inputs` entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub description: Option<String>,
    pub default: Option<String>,
    pub options: Vec<String>,
}

/// `~/.config/itse/config.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub usage: Option<bool>,
    pub daemon_idle_minutes: Option<u64>,
    pub server_idle_minutes: Option<u64>,
    pub mcp: McpConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub debug: bool,
}

/// The built-in extensions: launch data only, never pins (those live in a lock). A built-in is on
/// offer, not in use, until a layer names it in `use`.
pub const BUILT_INS: [(&str, &str); 7] = [
    ("bun", include_str!("../ext/bun/itse-ext.json")),
    ("claude-code", include_str!("../ext/claude-code/itse-ext.json")),
    ("gradle", include_str!("../ext/gradle/itse-ext.json")),
    ("jdk", include_str!("../ext/jdk/itse-ext.json")),
    ("kotlin", include_str!("../ext/kotlin/itse-ext.json")),
    ("node", include_str!("../ext/node/itse-ext.json")),
    ("rust", include_str!("../ext/rust/itse-ext.json")),
];

/// Template fields; one ending in `:` takes an argument. Any other `${…}` fails at load. `${ws}`
/// is the workspace id, so a server's own folder under `${cache}` is per workspace (servers only).
const FIELDS: [&str; 9] = ["pkg:", "consent:", "input:", "ready:", "workspaceFolder", "cache", "ws", "target_args", "filter_args"];

/// `s` with each `${field}` replaced by what `fill` gives for it; an unknown field, or one `fill`
/// has no value for, is an error that names it.
pub fn render(s: &str, fill: &mut dyn FnMut(&str) -> Option<String>) -> Result<String> {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(i) = rest.find("${") {
        let Some(len) = rest[i..].find('}') else { bail!("unclosed ${{ in '{s}'") };
        let key = &rest[i + 2..i + len];
        let known = |f: &&str| f.strip_suffix(':').map_or(key == *f, |f| key.strip_prefix(f).and_then(|a| a.strip_prefix(':')).is_some_and(|a| !a.is_empty()));
        ensure!(FIELDS.iter().any(known), "unknown template ${{{key}}} in '{s}'");
        out = out + &rest[..i] + &fill(key).ok_or_else(|| anyhow!("nothing fills ${{{key}}} in '{s}'"))?;
        rest = &rest[i + len + 1..];
    }
    Ok(out + rest)
}

/// `v` with each string in it rendered: a server's `init_options` and `settings` take templates too.
pub fn render_value(v: &Value, fill: &mut dyn FnMut(&str) -> Option<String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(render(s, fill)?),
        Value::Array(a) => Value::Array(a.iter().map(|x| render_value(x, fill)).collect::<Result<_>>()?),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), render_value(x, fill)?))).collect::<Result<_>>()?),
        other => other.clone(),
    })
}

/// An id that is safe as a file name: ASCII alphanumerics, `-` and `_`.
fn plain(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parse and check one extension: unknown keys (an action's VS Code UI keys aside) and unknown
/// template fields are errors that name where it came from.
pub fn parse_ext(text: &str, from: &str) -> Result<Extension> {
    let ext: Extension = serde_json::from_str(text).with_context(|| format!("extension {from}"))?;
    let check = || -> Result<()> {
        ensure!(ext.itse_ext == 1 && !ext.id.is_empty(), "itse_ext must be 1 and id set");
        for a in &ext.actions {
            if let Some(key) = a.extra.keys().find(|k| !VS_CODE_UI_KEYS.contains(&k.as_str())) {
                bail!("unknown field `{key}` in action '{}'", a.label);
            }
            let options = a.options.cwd.iter().chain(a.options.env.values());
            for t in [&a.command].into_iter().chain(&a.args).chain(&a.filter_args).chain(&a.target_args).chain(options) {
                render(t, &mut |_| Some(String::new()))?;
            }
        }
        for t in ext.servers.iter().flat_map(|s| s.command.iter().chain(s.env.values())) {
            render(t, &mut |_| Some(String::new()))?;
        }
        for v in ext.servers.iter().flat_map(|s| [&s.init_options, &s.settings]).chain(ext.debug.iter().flat_map(|d| [&d.arguments, &d.attach])) {
            render_value(v, &mut |_| Some(String::new()))?;
        }
        crate::parse::check(&ext)
    };
    check().with_context(|| format!("extension {from}"))?;
    Ok(ext)
}

/// One extension in use: its `use` settings, whether only the team layer names it (so it waits for
/// trust), and which extension `requires` it when no layer names it.
pub struct Use {
    pub id: String,
    pub settings: BTreeMap<String, Value>,
    pub team: bool,
    #[allow(dead_code)] // for `status` to say why an extension is in use
    pub by: Option<String>,
}

/// What the resolver reads: the extensions on offer, this repo's developer layer and this
/// worktree's team layer.
pub struct Manifest {
    /// The built-ins, and the extensions added with `itse ext add` (an added one replaces a built-in).
    pub exts: BTreeMap<String, (Extension, &'static str)>,
    pub dev: Option<(PathBuf, Layer)>,
    pub team: Option<Layer>,
    /// The team-layer files' bytes, for the trust hash; empty without a team layer.
    pub team_bytes: Vec<u8>,
    pub notes: Vec<String>,
}

impl Manifest {
    pub fn load(ws: &Workspace, dirs: &Dirs) -> Result<Self> {
        let mut exts = BTreeMap::new();
        for (id, text) in BUILT_INS {
            exts.insert(id.to_string(), (parse_ext(text, &format!("built-in {id}"))?, "built-in"));
        }
        let (added, mut notes) = (lock::added(dirs)?, Vec::new());
        for path in json_files(&dirs.config.join("extensions")) {
            let (id, bytes) = (path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), fs::read(&path)?);
            match added.get(&id) {
                Some(rec) if rec.sha256 == sys::sha256_hex(&bytes) => drop(exts.insert(id, (parse_ext(&String::from_utf8_lossy(&bytes), &path.display().to_string())?, "added"))),
                _ => notes.push(format!("{} is not loaded: it differs from what 'itse ext add' recorded", path.display())),
            }
        }
        let team_dir = ws.root.join(".itse");
        let (json, lock) = (fs::read(team_dir.join("itse.json")).ok(), fs::read(team_dir.join("itse.lock.json")).ok());
        let team = json.as_deref().map(|b| serde_json::from_slice::<Layer>(b).with_context(|| format!("team layer {}", team_dir.join("itse.json").display()))).transpose()?;
        let team_bytes = [json.unwrap_or_default(), lock.unwrap_or_default()].concat();
        Ok(Self { exts, dev: dev_layer(ws, dirs)?, team, team_bytes, notes })
    }

    /// A package by name, from whichever extension declares it.
    pub fn package(&self, name: &str) -> Option<&Package> {
        self.exts.values().find_map(|e| e.0.packages.get(name))
    }

    /// The extensions in use: the team layer's, the developer layer's over them by name, then what
    /// those `require`.
    pub fn uses(&self) -> Vec<Use> {
        let mut out: Vec<Use> = Vec::new();
        for (layer, team) in [(self.team.as_ref(), true), (self.dev.as_ref().map(|d| &d.1), false)] {
            for (id, settings) in layer.into_iter().flat_map(|l| &l.uses) {
                out.retain(|u| &u.id != id);
                out.push(Use { id: id.clone(), settings: settings.clone(), team, by: None });
            }
        }
        let mut i = 0;
        while let Some((id, team)) = out.get(i).map(|u| (u.id.clone(), u.team)) {
            for r in self.exts.get(&id).map(|e| e.0.requires.clone()).unwrap_or_default() {
                if !out.iter().any(|u| u.id == r) {
                    out.push(Use { id: r, settings: BTreeMap::new(), team, by: Some(id.clone()) });
                }
            }
            i += 1;
        }
        out
    }

    /// Extensions not in use with a `detect` file among the root's top-level names: (id, file).
    pub fn detected(&self, root: &Path, uses: &[Use]) -> Vec<(String, String)> {
        let unused = self.exts.iter().filter(|(id, _)| !uses.iter().any(|u| &u.id == *id));
        unused.filter_map(|(id, (ext, _))| ext.detect.iter().find(|f| !f.contains('/') && root.join(f).exists()).map(|f| (id.clone(), f.clone()))).collect()
    }

    /// The packages in use that a server's command and env name through `${pkg:x}`.
    pub fn packages_of<'a>(&'a self, s: &Server, uses: &'a [Use]) -> impl Iterator<Item = &'a Package> {
        let mut names = Vec::new();
        for t in s.command.iter().chain(s.env.values()) {
            let _ = render(t, &mut |key| {
                names.extend(key.strip_prefix("pkg:").map(String::from));
                Some(String::new())
            });
        }
        names.into_iter().filter_map(move |n| uses.iter().filter_map(|u| self.exts.get(&u.id)).find_map(|e| e.0.packages.get(&n)))
    }

    /// One map of the layers merged by name, the developer's entry over the team's: name → (entry, from the team).
    pub fn merged<'a, T>(&'a self, of: impl Fn(&'a Layer) -> &'a BTreeMap<String, T>) -> BTreeMap<&'a String, (&'a T, bool)> {
        let layers = [(self.team.as_ref(), true), (self.dev.as_ref().map(|d| &d.1), false)];
        layers.into_iter().flat_map(|(l, team)| l.into_iter().flat_map(&of).map(move |(k, v)| (k, (v, team)))).collect()
    }
}

/// The developer layer of this repo: the file under `config/workspaces/` whose `repo` is the repo
/// key. The others are only peeked at, so one broken file can't break every repo.
fn dev_layer(ws: &Workspace, dirs: &Dirs) -> Result<Option<(PathBuf, Layer)>> {
    for path in json_files(&dirs.config.join("workspaces")) {
        let bytes = fs::read(&path)?;
        if serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["repo"] == *ws.repo.to_string_lossy()) {
            return Ok(Some((path.clone(), serde_json::from_slice(&bytes).with_context(|| format!("developer layer {}", path.display()))?)));
        }
    }
    Ok(None)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    files
}

/// `itse use <ext> [key=value…]` and `itse unuse <ext>`: change `use.<ext>` in this repo's
/// developer layer, made 0600 in a 0700 dir with `repo` set, and removed once empty. Nothing else
/// is written, nothing starts, and trust is untouched. Values stay strings, so `version=1.10`
/// keeps its zero.
pub fn use_ext(ws: &Workspace, dirs: &Dirs, args: &Args, add: bool) -> Result<Outcome> {
    // An flock on the workspaces dir serializes the read-modify-write: parallel MCP tool calls, or
    // the CLI and MCP at once, would otherwise drop each other's entries or pick the same file.
    let dir = dirs.config.join("workspaces");
    if add {
        sys::private_dir(&dir)?;
    }
    let _held = fs::File::open(&dir).ok().map(|f| f.lock().map(|()| f)).transpose().with_context(|| format!("locking {}", dir.display()))?;
    let (m, id) = (Manifest::load(ws, dirs)?, args.target.clone().unwrap_or_default());
    let (path, mut doc) = match &m.dev {
        Some((path, _)) => (path.clone(), serde_json::from_slice::<Value>(&fs::read(path)?)?),
        None => {
            let name = ws.repo_name().file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
            let path = (1..).map(|n| dir.join(if n == 1 { format!("{name}.json") } else { format!("{name}-{n}.json") })).find(|p| !p.exists()).unwrap_or_default();
            (path, json!({"itse": 1, "repo": ws.repo.to_string_lossy(), "use": {}}))
        }
    };
    let what = if add {
        ensure!(m.exts.contains_key(&id), "no extension '{id}'; 'itse ext list' lists them");
        let mut settings = Map::new();
        for kv in &args.rest {
            let (k, v) = kv.split_once('=').ok_or_else(|| anyhow!("'{kv}' is not key=value"))?;
            ensure!(matches!((k, v), ("install", "fetch" | "path") | ("version", _)), "unknown setting '{kv}' for {id}; the settings are install=fetch|path and version=V");
            settings.insert(k.into(), v.into());
        }
        doc["use"][&id] = Value::Object(settings);
        format!("use.{id} = {}", doc["use"][&id])
    } else {
        if doc["use"].as_object_mut().and_then(|u| u.remove(&id)).is_none() {
            ensure!(!m.team.as_ref().is_some_and(|t| t.uses.contains_key(&id)), "{id} is in the team layer .itse/itse.json, which itse never writes");
            bail!("{id} is not in the developer layer");
        }
        format!("removed use.{id}")
    };
    serde_json::from_value::<Layer>(doc.clone()).with_context(|| format!("developer layer {}", path.display()))?;
    let keys_left = doc.as_object().is_some_and(|d| d.keys().any(|k| !matches!(k.as_str(), "itse" | "repo" | "use")));
    if doc["use"].as_object().is_some_and(Map::is_empty) && !keys_left {
        fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
    } else {
        lock::write(&path, &doc)?;
    }
    Ok(Outcome { notes: vec![what], ..Outcome::new(if add { "use" } else { "unuse" }, Verdict::Ok, id, path.display().to_string()) })
}

/// `itse ext list|show <id>|add <file>|remove <id>`. `add` copies the file to
/// `config/extensions/<id>.json` and records its sha256 and actor; only a file that still has that
/// hash loads. `cwd` is the caller's, for a relative file.
pub fn ext(ws: &Workspace, dirs: &Dirs, cwd: &Path, args: &Args, actor: &str) -> Result<Outcome> {
    let (m, arg) = (Manifest::load(ws, dirs)?, args.rest.first().cloned().unwrap_or_default());
    let uses = m.uses();
    let notes = match args.target.as_deref().unwrap_or("list") {
        "list" => m
            .exts
            .iter()
            .map(|(id, (e, origin))| {
                format!("{id:<12} {:<8} {origin:<8} {}", e.version.as_deref().unwrap_or("-"), if uses.iter().any(|u| &u.id == id) { "in use" } else { "available" })
            })
            .collect(),
        "show" => {
            let (e, origin) = m.exts.get(&arg).ok_or_else(|| anyhow!("no extension '{arg}'; 'itse ext list' lists them"))?;
            let list = |name: &str, items: Vec<String>| (!items.is_empty()).then(|| format!("{name}: {}", items.join(", ")));
            let ignored = e.actions.iter().flat_map(|a| a.extra.keys().cloned().chain(a.options.shell.as_ref().map(|_| "options.shell".into()))).collect();
            let packages = e.packages.iter().map(|(n, p)| format!("{n} ({}{})", p.install, if p.executes_project_code { ", runs project code" } else { "" })).collect();
            [
                Some(format!("{arg} {} ({origin})", e.version.as_deref().unwrap_or("-"))),
                list("detect", e.detect.clone()),
                list("requires", e.requires.clone()),
                list("packages", packages),
                list("servers", e.servers.iter().map(|s| s.id.clone()).collect()),
                list("actions", e.actions.iter().map(|a| a.label.clone()).collect()),
                list("problemMatchers", e.problem_matchers.keys().cloned().collect()),
                list("ignored", ignored),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        "add" => {
            let bytes = fs::read(cwd.join(&arg)).with_context(|| format!("reading {arg}"))?;
            let e = parse_ext(&String::from_utf8_lossy(&bytes), &arg)?;
            ensure!(plain(&e.id), "extension id '{}' is not a plain name", e.id);
            lock::write_bytes(&dirs.config.join("extensions").join(format!("{}.json", e.id)), &bytes)?;
            vec![format!("added {} (sha256 {}) by {actor}", e.id, lock::record_added(dirs, &e.id, &bytes, actor)?)]
        }
        "remove" => {
            ensure!(plain(&arg), "extension id '{arg}' is not a plain name");
            let path = dirs.config.join("extensions").join(format!("{arg}.json"));
            ensure!(lock::record_removed(dirs, &arg)? | fs::remove_file(&path).is_ok(), "no added extension '{arg}'; built-ins can't be removed");
            vec![format!("removed {arg} by {actor}")]
        }
        "tools" => bail!("ext tools arrives with hosted MCP servers"),
        other => bail!("unexpected argument '{other}'; 'ext' takes list, show, tools, add or remove"),
    };
    let scope = [args.target.as_deref().unwrap_or("list"), &arg].join(" ").trim_end().to_string();
    Ok(Outcome { notes, ..Outcome::new("ext", Verdict::Ok, scope, "") })
}

#[cfg(test)]
mod tests;
