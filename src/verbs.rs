//! The verb table and dispatch. One row per verb drives the CLI parser, the MCP
//! tools, the generated index and the usage log's closed verb set. MCP's `args` are parsed by the
//! same function as the CLI's arguments, so every flag works unchanged over both.
//!
//! A daemon verb whose body is not built yet answers `ERROR … not built yet`. Built now: `status`,
//! `show` and the index, and through the CLI `daemon`, `mcp` and `stats`; the manifest's verbs:
//! `use`, `unuse`, `ext`, `trust` and `consent`, the store's: `install`, `lock` and `gc`, and status
//! rows; `run`, `build`, `test`, `cancel` and `stop` (in `actions`), `problems` over action
//! results (in `parse`) and the language servers' diagnostics, `find`, `refs`, `info` and
//! `outline` on the language servers (in `lsp`), `debug` on debug adapters (in `dap`), `ext tools`
//! on hosted MCP servers (in `mcp::client`), and `integrate` and `hook` for harnesses (in `harness`).

pub mod harness;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, process};

use anyhow::{Result, bail, ensure};
use lexopt::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::daemon::{VERSION, client, proto};
use crate::host::{self, Host};
use crate::resolve::{Resolved, resolve, target};
use crate::results::{self, DETAIL, HOOK, Outcome, State, Store, VERB, Verdict, text};
use crate::rpc::Cancel;
use crate::store::{self, Delegate};
use crate::sys;
use crate::usage::{Call, Event, Ident, Usage};
use crate::workspace::{Dirs, Workspace};
use crate::{actions, env, lock, manifest, parse, pins};

/// Which MCP tool carries a verb (names in `mcp::tool_name`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Query,
    Run,
    Show,
    Debug,
    CliOnly,
}

/// `Local` verbs run in the CLI process and never start the daemon.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Runs {
    Daemon,
    Local,
}

/// A verb's arguments: the first positional is `target`, the others and any `Rest` go to `rest`;
/// flags go to `flags` by name: `Flag` as true, `Opt` as its value, `Many` as a list of values.
/// A `Req` positional must be given, and comes before any `Pos`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Param {
    Req(&'static str),
    Pos(&'static str),
    Rest(&'static str),
    Flag(&'static str),
    Opt(&'static str),
    Many(&'static str),
}

#[derive(Debug)]
pub struct Verb {
    pub name: &'static str,
    pub tool: Tool,
    pub params: &'static [Param],
    pub runs: Runs,
    pub human_only: bool,
    pub about: &'static str,
}

const fn verb(name: &'static str, tool: Tool, runs: Runs, params: &'static [Param], about: &'static str) -> Verb {
    Verb { name, tool, params, runs, human_only: false, about }
}

const fn human(v: Verb) -> Verb {
    Verb { human_only: true, ..v }
}

use Param::{Flag, Many, Opt, Pos, Req, Rest};
use Runs::{Daemon, Local};

/// Every verb; names starting with `__` are internal and left out of help and the index.
pub const VERBS: &[Verb] = &[
    verb("status", Tool::Query, Daemon, &[Flag("hosts")], "the workspace, its items and their states"),
    verb("use", Tool::Run, Local, &[Req("ext"), Rest("key=value")], "add an extension to the developer layer"),
    verb("unuse", Tool::CliOnly, Local, &[Req("ext")], "remove an extension from the developer layer"),
    verb("install", Tool::Run, Daemon, &[Rest("pkg")], "fetch and verify the pinned packages, as a run"),
    verb("lock", Tool::Run, Daemon, &[Flag("refresh"), Opt("target"), Pos("pkg")], "refresh pins from their sources; installs nothing"),
    human(verb("trust", Tool::CliOnly, Daemon, &[Flag("revoke")], "trust this workspace to run project code")),
    human(verb("consent", Tool::CliOnly, Daemon, &[Req("pkg"), Opt("eula-hash"), Opt("region"), Opt("data-sharing")], "record consent to a package's terms")),
    verb("ext", Tool::Query, Daemon, &[Pos("list|show|tools|add|remove"), Pos("id")], "list, show, add or remove extensions"),
    verb("env", Tool::CliOnly, Local, &[Flag("sh")], "the composed environment"),
    verb("exec", Tool::CliOnly, Local, &[Rest("cmd")], "run a command in the composed environment"),
    verb("gc", Tool::CliOnly, Daemon, &[Flag("keep-locked")], "remove unused packages and old results"),
    verb("run", Tool::Run, Daemon, &[Req("label"), Many("input"), Rest("args")], "run a declared action"),
    verb("build", Tool::Run, Daemon, &[Pos("target"), Opt("action")], "build; located problems"),
    verb("test", Tool::Run, Daemon, &[Pos("target"), Pos("filter"), Opt("action")], "run tests; failing tests at their lines"),
    verb("problems", Tool::Query, Daemon, &[Pos("scope"), Flag("new"), Flag("all")], "compiler and language server problems"),
    verb("find", Tool::Query, Daemon, &[Req("name"), Opt("kind")], "symbols by name"),
    verb("info", Tool::Query, Daemon, &[Req("name")], "a symbol's signature, docs and location"),
    verb("refs", Tool::Query, Daemon, &[Req("name")], "references to a symbol, by file"),
    verb("outline", Tool::Query, Daemon, &[Req("path")], "a file's symbols"),
    verb(
        "debug",
        Tool::Debug,
        Daemon,
        &[Pos("test|attach|step|continue|eval|snapshot|stop"), Rest("args"), Many("at"), Many("log"), Opt("frame")],
        "debug a test: test [target] [filter|run:N/T1], attach <host:port>, step <dbg> [over|in|out], continue|snapshot|stop <dbg>, eval <dbg> <expr>",
    ),
    verb("show", Tool::Show, Daemon, &[Req("id"), Opt("page"), Flag("raw"), Opt("grep"), Opt("tail"), Flag("json")], "a stored result or log by id"),
    verb("stop", Tool::Run, Daemon, &[Req("handle")], "stop a server or debug session"),
    verb("cancel", Tool::Run, Daemon, &[Req("run")], "cancel a run"),
    verb("stats", Tool::CliOnly, Local, &[Opt("since"), Opt("by"), Flag("json")], "the local usage log, summed"),
    verb("verbs", Tool::CliOnly, Local, &[Flag("json")], "the verb index"),
    human(verb("integrate", Tool::CliOnly, Local, &[Req("ext"), Flag("skill"), Flag("mcp"), Opt("recipe"), Flag("remove"), Opt("home"), Flag("yes")], "set up a harness")),
    verb("hook", Tool::CliOnly, Local, &[Req("recipe")], "a harness hook; never starts the daemon"),
    verb("mcp", Tool::CliOnly, Local, &[], "the MCP endpoint on stdio"),
    verb("daemon", Tool::CliOnly, Local, &[Pos("stop|restart|status")], "the workspace daemon"),
    verb("__daemonize", Tool::CliOnly, Local, &[], "start the daemon in its own session"),
    verb("__run", Tool::CliOnly, Local, &[Pos("rundir")], "the runner of one action"),
];

pub fn find(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.name == name)
}

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Args {
    pub target: Option<String>,
    pub rest: Vec<String>,
    pub flags: BTreeMap<String, Value>,
}

/// The tokens after the verb, from the CLI or from MCP's `args`.
pub fn parse_args(verb: &Verb, tokens: impl IntoIterator<Item = OsString>) -> Result<Args> {
    let (mut parser, mut args) = (lexopt::Parser::from_args(tokens), Args::default());
    let positional = verb.params.iter().filter(|p| matches!(p, Req(_) | Pos(_))).count();
    let rest = verb.params.iter().any(|p| matches!(p, Rest(_)));
    while let Some(arg) = parser.next()? {
        match arg {
            Long(name) => {
                let name = name.to_string();
                match verb.params.iter().find(|p| matches!(p, Flag(n) | Opt(n) | Many(n) if *n == name)) {
                    Some(Flag(_)) => drop(args.flags.insert(name, true.into())),
                    Some(Opt(_)) => drop(args.flags.insert(name, parser.value()?.string()?.into())),
                    Some(_) => {
                        let value = parser.value()?.string()?;
                        if let Value::Array(list) = args.flags.entry(name).or_insert_with(|| json!([])) {
                            list.push(value.into());
                        }
                    }
                    None => bail!("invalid option '--{name}'"),
                }
            }
            Value(v) if args.target.is_none() && positional > 0 => args.target = Some(v.string()?),
            Value(v) if rest || args.rest.len() + 1 < positional => args.rest.push(v.string()?),
            _ => return Err(arg.unexpected().into()),
        }
    }
    let given = usize::from(args.target.is_some()) + args.rest.len();
    if let Some(Req(name)) = verb.params.iter().filter(|p| matches!(p, Req(_) | Pos(_))).nth(given).filter(|p| matches!(p, Req(_))) {
        bail!("missing argument <{name}>");
    }
    Ok(args)
}

/// Who is calling, from `hello`: the mode, the caller's env (the base env of actions the call
/// starts, never written to disk or logged), and the ids derived from it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Caller {
    pub mode: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: PathBuf,
    /// The CLI's own binary, which shims link to.
    #[serde(default)]
    pub exe_path: PathBuf,
    #[serde(skip)]
    pub id: Ident,
}

/// The dispatch wrapper of every daemon call, from the CLI, a hook or MCP: look the verb up,
/// parse its tokens, refuse what this caller may not run, run it, shape, store and render the
/// result, and record the call in the usage log. Answers `{exit, text}`.
pub fn call(host: &Arc<Host>, caller: &Caller, params: &Value, cancel: &Cancel) -> Value {
    let (started, name) = (Instant::now(), params["verb"].as_str().unwrap_or(""));
    let verb = find(name).filter(|v| !v.name.starts_with("__"));
    let page = params["page"].as_u64().map_or(1, |p| p.min(u32::MAX.into()) as u32);
    let store = Store(host.ws.state_dir(&host.dirs));
    let (via, session) = (caller.mode.as_str(), caller.id.session.as_str());
    let wait = Duration::from_secs(params["wait"].as_u64().unwrap_or(if via == "mcp" { 50 } else { 100 }));
    let sh = |o, n| shape(o, &store, via, session, n);
    let answer = match verb.map(|v| (v, args(v, &params["args"]))) {
        None => sh(Outcome::error(name, "unknown_verb", "not a daemon verb"), 1),
        Some((v, Err(e))) => sh(Outcome::error(v.name, "bad_args", format!("{e}")), 1),
        Some((v, Ok(args))) if via == "mcp" && (v.human_only || (v.name == "ext" && matches!(args.target.as_deref(), Some("add" | "remove")))) => {
            let tokens: Vec<String> = [v.name].into_iter().chain(params["args"].as_array().into_iter().flatten().filter_map(Value::as_str)).map(String::from).collect();
            let counts = text("human", &[("cmd", &crate::mcp::next(&tokens, via, true))]);
            sh(Outcome { err: Some("needs_human"), ..Outcome::new(v.name, Verdict::NeedsHuman, "", counts) }, 1)
        }
        Some((v, Ok(_))) if via == "mcp" && v.tool == Tool::CliOnly => sh(Outcome::error(v.name, "bad_args", text("cli_only", &[])), 1),
        // A tool carries the verbs of its enum only, so `debug` stays out of reach until opted in.
        Some((v, Ok(_))) if via == "mcp" && params["tool"] != crate::mcp::tool_name(v.tool) => {
            sh(Outcome::error(v.name, "bad_args", text("wrong_tool", &[("tool", &params["tool"].as_str().unwrap_or(""))])), 1)
        }
        Some((v, Ok(args))) => match v.name {
            "status" => sh(status(host, caller, &args, cancel), page),
            "show" => results::show(&store, &args, via, session),
            "install" | "lock" | "gc" => sh(store_verb(&host.ws, &host.dirs, caller, v.name, &args, cancel, &store::Real(caller.env.clone())), page),
            "run" | "build" | "test" => sh(run(host, caller, v.name, &args, wait, cancel).unwrap_or_else(|e| Outcome::error(v.name, "bad_args", format!("{e:#}"))), page),
            "problems" => {
                let (scope, all, new) = (args.target.as_deref().unwrap_or(""), args.flags.contains_key("all"), args.flags.contains_key("new"));
                // A debug-only seam for the tests of a server that doesn't settle in time.
                let ms = caller.env.get("ITSE_TEST_SETTLE_MS").filter(|_| cfg!(debug_assertions)).and_then(|ms| ms.parse().ok());
                // A hook waits 1.5 s at most: on a small crate rust-analyzer's push that says what an edit
                // to a file no query opened did comes under 0.2 s after it (p95), the hook's whole call
                // under 0.6 s with the settle's quiet; a bigger crate's check past that is unsettled.
                let wait = Duration::from_millis(ms.unwrap_or(if via == "hook" { 1500 } else { 20_000 }));
                let mut lsp = crate::lsp::problems(host, &caller.env, scope, via != "hook", wait, cancel);
                lsp.settled &= !cancel.is_cancelled();
                let mut o = parse::problems(&store, scope, all, new, &format!("{via}:{session}"), &mut lsp).map(|o| Outcome {
                    warm: crate::lsp::warm(&lsp.index),
                    counts: lsp.untrusted.iter().cloned().chain([o.counts.clone()]).chain(lsp.index).collect::<Vec<_>>().join("; "),
                    // Before trust a server in scope never ran, so its problems are unknown, not none.
                    verdict: if lsp.untrusted.is_some() { Verdict::Untrusted } else { o.verdict },
                    err: if lsp.untrusted.is_some() { Some("untrusted") } else { o.err },
                    states: lsp.states,
                    notes: o.notes.iter().cloned().chain(lsp.notes).collect(),
                    ..o
                });
                // A hook passes on errors and warnings only; with neither, it has nothing to say.
                if let Ok(o) = o.as_mut()
                    && via == "hook"
                {
                    o.items.retain(|i| i.kind == "error" || i.kind == "warning");
                    o.verdict = if o.items.is_empty() && o.verdict != Verdict::Untrusted { Verdict::Empty } else { o.verdict };
                }
                sh(o.unwrap_or_else(|e| Outcome::error("problems", "io", format!("{e:#}"))), page)
            }
            "cancel" | "stop" => sh(stop(host, v.name, args.target.as_deref().unwrap_or_default()), page),
            "debug" => sh(crate::dap::debug(host, caller, &args, wait, cancel), page),
            "find" => sh(crate::lsp::find(host, &caller.env, args.target.as_deref().unwrap_or_default(), args.flags.get("kind").and_then(Value::as_str), cancel), page),
            "refs" => sh(crate::lsp::refs(host, &caller.env, session, args.target.as_deref().unwrap_or_default(), cancel).unwrap_or_else(|o| *o), page),
            "info" => sh(crate::lsp::info(host, &caller.env, session, args.target.as_deref().unwrap_or_default(), cancel).unwrap_or_else(|o| *o), page),
            "outline" => sh(crate::lsp::outline(host, &caller.env, &caller.cwd, args.target.as_deref().unwrap_or_default(), cancel).unwrap_or_else(|o| *o), page),
            "ext" if args.target.as_deref() == Some("tools") => sh(crate::mcp::client::ext_tools(host, caller, args.rest.first().map_or("", String::as_str), cancel), page),
            "use" | "unuse" | "ext" | "trust" | "consent" => {
                let o = manifest_verb(&host.ws, &host.dirs, &caller.env, &caller.cwd, v.name, &args, params["tty"] == true);
                // A revoked trust or an unused extension leaves a server that may no longer run: it stops now.
                let _ = crate::lsp::servers(host, &caller.env, v.name, "", &|_| false, false, cancel);
                let _ = crate::mcp::client::resolved(host, &caller.env);
                crate::dap::untrusted(host, &caller.env);
                sh(o, page)
            }
            _ => sh(Outcome::error(v.name, "internal", text("not_built", &[])), 1),
        },
    };
    let ms = started.elapsed().as_millis() as u64;
    record(&host.usage, caller, verb.map_or("unknown", |v| v.name), None, ms, &answer);
    json!({"exit": exit(&answer), "text": answer.text, "verdict": answer.verdict, "settled": answer.settled})
}

/// What a caller gets back: the text, and what the usage log counts about it.
#[derive(Default)]
pub struct Answer {
    pub verdict: Verdict,
    pub text: String,
    pub full: u64,
    pub trunc: bool,
    pub err: Option<&'static str>,
    pub warm: Option<bool>,
    pub settled: Option<bool>,
}

impl Answer {
    pub fn failed(o: Outcome) -> Self {
        Self { verdict: Verdict::Error, text: o.head() + "\n", err: o.err, ..Self::default() }
    }
}

/// Shape an outcome for its caller: page `n` under the verb cap (a hook's own cap for a hook); a
/// result that has items or more than one page is stored as `q:N`, so its pointers lead somewhere.
fn shape(mut o: Outcome, store: &Store, via: &str, session: &str, n: u32) -> Answer {
    let cap = if via == "hook" { HOOK } else { VERB };
    let mut text = o.render(via, n, cap, false);
    if o.id.is_none() && (!o.items.is_empty() || o.page.of > 1) {
        o.id = host::Next::take(&store.0, "q").inspect_err(|e| eprintln!("itse daemon: results: {e:#}")).ok();
        text = o.render(via, n, cap, false);
    }
    let text = match text {
        Ok(text) => text,
        Err(line) => return Answer::failed(Outcome::error(&o.verb, "bad_args", line)),
    };
    if o.id.as_deref().is_some_and(|id| id.starts_with("q:")) {
        let _ = store.save(&o, session).inspect_err(|e| eprintln!("itse daemon: results: {e:#}"));
    }
    let trunc = o.page.of > 1 || o.items.iter().any(|i| i.detail.len() > DETAIL);
    Answer { verdict: o.verdict, full: o.full.bytes, trunc, err: o.err, text, warm: o.warm, settled: o.settled }
}

/// A verb's tokens: from MCP's `args` or the CLI's (both strings for the same parser), or parsed `Args`.
fn args(verb: &Verb, args: &Value) -> Result<Args> {
    match args {
        Value::Array(tokens) if tokens.iter().all(Value::is_string) => parse_args(verb, tokens.iter().map(|t| OsString::from(t.as_str().unwrap_or("")))),
        Value::Array(_) => bail!("args must be strings"),
        Value::Null => Ok(Args::default()),
        v => Ok(serde_json::from_value::<Args>(v.clone())?),
    }
}

/// 0 ok or empty, 1 the verb reports failure (a name lookup that found nothing too), 2 error, 3 a
/// human or a door must act, 4 running, 130 cancelled.
fn exit(a: &Answer) -> u8 {
    match a.verdict {
        _ if a.err == Some("cancel") => 130,
        Verdict::Fail => 1,
        // A debug start that never stopped did not do what it was for.
        Verdict::Exited if ["EXITED debug test", "EXITED debug attach"].iter().any(|p| a.text.starts_with(p)) => 1,
        Verdict::Empty if a.text.split(' ').nth(1).is_some_and(|v| matches!(v, "find" | "info" | "refs")) => 1,
        Verdict::Error => 2,
        Verdict::NeedsHuman | Verdict::NeedsConsent | Verdict::Untrusted | Verdict::Blocked => 3,
        Verdict::Running => 4,
        _ => 0,
    }
}

/// One `call` line in the usage log: the verb's name from the table (or a closed class), never the
/// caller's string; `ext` is a built-in extension's id.
pub fn record(usage: &Usage, caller: &Caller, verb: &str, ext: Option<String>, ms: u64, a: &Answer) {
    let outcome = match (a.verdict, a.err) {
        (_, Some("cancel")) => "cancel",
        (_, Some("timeout")) => "timeout",
        (Verdict::Fail, _) => "fail",
        (_, Some(_)) | (Verdict::Error, _) => "error",
        _ => "ok",
    };
    let via = ["mcp", "hook"].into_iter().find(|m| *m == caller.mode).unwrap_or("cli");
    let (err, bytes) = (a.err.filter(|e| !matches!(*e, "cancel" | "timeout")), a.text.len() as u64);
    let (client, client_v) = (caller.id.client.clone(), caller.id.client_v.clone());
    let call = Call { via, client, client_v, verb: verb.into(), ext, ms, outcome, err, bytes, full: a.full.max(bytes), trunc: a.trunc, warm: a.warm, settled: a.settled };
    usage.record(&caller.env, &caller.id.sess, Event::Call(call));
}

/// `run <label> [--input k=v…] [-- args]`, and `build`/`test` on their action (`pick`) with the
/// target and filter as inputs: refuse project code in an untrusted workspace, start the action
/// with the caller's env composed, and wait up to `wait` for its end or its `ready` line. An ended
/// run answers with its parsed result, stored under the caller's session. A Ctrl-C cancels the run; a caller
/// that only went away leaves it running. Runs of one label may overlap; the answer names
/// the others still going.
fn run(host: &Arc<Host>, caller: &Caller, verb: &str, args: &Args, wait: Duration, cancel: &Cancel) -> Result<Outcome> {
    let (r, flag) = (resolve(&host.ws, &host.dirs, &caller.env, SystemTime::now())?, |k: &str| args.flags.get(k).and_then(Value::as_array).into_iter().flatten());
    let uses = r.manifest.uses();
    let declared: Vec<&manifest::Action> = uses.iter().filter_map(|u| r.manifest.exts.get(&u.id)).flat_map(|e| &e.0.actions).collect();
    let (label, inputs, extra): (_, BTreeMap<String, String>, _) = match verb {
        "run" => {
            (args.target.clone().unwrap_or_default(), flag("input").filter_map(|kv| kv.as_str()?.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect(), &args.rest[..])
        }
        _ => match pick(&declared, verb, args) {
            Ok(label) => (label, [("target", args.target.clone()), ("filter", args.rest.first().cloned())].into_iter().filter_map(|(k, v)| Some((k.into(), v?))).collect(), &[]),
            Err(o) => return Ok(*o),
        },
    };
    let plan = actions::plan(&r, &host.ws.root, &host.dirs.cache, &label, &inputs, extra)?;
    if plan.code && !r.trusted {
        let states = r.states.into_iter().filter(|s| s.state == "untrusted").collect();
        return Ok(Outcome { states, err: Some("untrusted"), ..Outcome::new(verb, Verdict::Untrusted, label, "it runs project code") });
    }
    let mut env = env::workspace_env(&r, &host.dirs, caller.env.clone(), &store::Real(caller.env.clone()))?;
    env.extend(plan.env.clone());
    let same = |r: &&Arc<actions::Run>| actions::RunRecord::load(&r.dir).is_some_and(|rec| rec.spec.label == label);
    let others: Vec<String> = sys::lock(&host.runs).values().filter(same).map(|r| format!("{} of {label} is running too", r.handle)).collect();
    let run = actions::start(host, &label, &plan, &env)?;
    let ready = actions::wait(&run, plan.ready.as_ref(), wait, cancel).map(|caps| caps.iter().enumerate().map(|(i, c)| format!(" ${{ready:{}}}={c}", i + 1)).collect::<String>());
    let interrupted = cancel.1.load(std::sync::atomic::Ordering::SeqCst);
    if interrupted {
        actions::stop_run(&run);
    }
    // An ended run was parsed and stored by its watch thread; it is stored again as this verb's.
    let store = Store(host.ws.state_dir(&host.dirs));
    let ended = run.ended(Duration::ZERO);
    let mut o = store.load(&run.handle).ok().filter(|_| ended).map_or_else(|| actions::outcome(verb, &run.handle, &run.dir), |(o, _)| Outcome { verb: verb.into(), ..o });
    if verb != "run" {
        o.scope = format!("{} ({label})", args.target.clone().unwrap_or_else(|| host.ws.root.file_name().unwrap_or_default().to_string_lossy().into()));
    }
    if ended {
        let _ = store.save(&o, &caller.id.session).inspect_err(|e| eprintln!("itse daemon: results: {e:#}"));
    }
    o.notes.splice(0..0, others);
    Ok(Outcome { counts: ready.map_or(o.counts.clone(), |caps| format!("ready{caps}")), err: if interrupted { Some("cancel") } else { o.err }, ..o })
}

/// The action `build` or `test` runs: `--action L`, else the one default of its kind among the
/// extensions in use. Several defaults are an error with each as a candidate.
fn pick(declared: &[&manifest::Action], verb: &str, args: &Args) -> Result<String, Box<Outcome>> {
    if let Some(label) = args.flags.get("action").and_then(Value::as_str) {
        return Ok(label.into());
    }
    let mut defaults: Vec<&str> =
        declared.iter().filter(|a| matches!(&a.group, Some(manifest::Group::Full { kind, is_default: true }) if kind == verb)).map(|a| a.label.as_str()).collect();
    defaults.sort_unstable();
    let next =
        |l: &str| State { item: l.into(), state: "candidate".into(), actor: Some("agent".into()), next: Some(vec![verb.into(), "--action".into(), l.into()]), ..State::default() };
    match defaults[..] {
        [one] => Ok(one.into()),
        [] => Err(Box::new(Outcome::error(verb, "bad_args", format!("no default {verb} action in the extensions in use; 'itse run <label>' runs a declared one")))),
        _ => {
            let o = Outcome::error(verb, "bad_args", format!("{} default {verb} actions: {} (--action L)", defaults.len(), defaults.join(", ")));
            Err(Box::new(Outcome { states: defaults.iter().map(|l| next(l)).collect(), ..o }))
        }
    }
}

/// `cancel run:N` and `stop <handle>`: a live run gets the escalation (and answers how it ended,
/// as does one that already had); a server is stopped, and a debug session disconnected.
pub fn stop(host: &Host, verb: &str, target: &str) -> Outcome {
    if verb == "stop" && target.starts_with("dbg:") {
        return crate::dap::stop(host, verb, target);
    }
    let Some(n) = target.strip_prefix("run:").filter(|n| n.parse::<u64>().is_ok()) else {
        // A server's bare id (`rust-analyzer`) names its handle when only one has it.
        let named: Vec<String> = sys::lock(&host.slots).keys().filter(|h| h.split_once(':').is_some_and(|(_, id)| id == target)).cloned().collect();
        let target = if let [one] = &named[..] { one.as_str() } else { target };
        if verb == "stop" && host.stop_server(target, Duration::from_secs(5)) {
            return Outcome::new(verb, Verdict::Stopped, target, "stopped");
        }
        let state = sys::lock(&host.slots).get(target).map_or(String::new(), |s| format!(" ({})", s.state()));
        return Outcome::error(verb, "unknown_id", format!("{target} is not running{state}"));
    };
    let live = sys::lock(&host.runs).get(target).cloned();
    live.inspect(|run| actions::stop_run(run));
    let dir = host.ws.state_dir(&host.dirs).join("runs").join(n);
    if dir.join("run.json").exists() { actions::outcome(verb, target, &dir) } else { Outcome::error(verb, "unknown_id", format!("no run {target}")) }
}

/// The manifest's verbs, from the daemon or (`use`, `unuse`) the CLI. `tty` says whether the CLI
/// had a terminal: human-only verbs record it, and `consent` refuses without one.
pub fn manifest_verb(ws: &Workspace, dirs: &Dirs, env: &BTreeMap<String, String>, cwd: &Path, name: &str, args: &Args, tty: bool) -> Outcome {
    let actor = if tty { "tty" } else { "non-tty" };
    let done = match name {
        "use" | "unuse" => manifest::use_ext(ws, dirs, args, name == "use").and_then(|o| Ok(Outcome { states: resolve(ws, dirs, env, SystemTime::now())?.states, ..o })),
        "trust" => resolve(ws, dirs, env, SystemTime::now()).and_then(|r| lock::trust(dirs, ws, &r.trust_hash, args.flags.contains_key("revoke"), actor)),
        "consent" => resolve(ws, dirs, env, SystemTime::now()).and_then(|r| r.consent(dirs, args, tty)),
        _ => manifest::ext(ws, dirs, cwd, args, actor),
    };
    done.unwrap_or_else(|e| Outcome::error(name, "bad_args", format!("{e:#}")))
}

/// The store's verbs: `install`, `lock` and `gc --keep-locked`, with the caller's env for
/// the delegates (`Real` gets the door's proxy settings from it). A door refusal is BLOCKED in both
/// `install` and `lock`, since only the halti owner can open the host.
pub fn store_verb(ws: &Workspace, dirs: &Dirs, caller: &Caller, name: &str, args: &Args, cancel: &Cancel, d: &dyn Delegate) -> Outcome {
    let root = ws.root.display().to_string();
    let verdict = |rows: &[State]| match (rows.iter().any(|s| s.state == "blocked"), rows.iter().any(|s| s.state == "failed")) {
        (true, _) => (Verdict::Blocked, Some("blocked_by_door")),
        (_, true) => (Verdict::Fail, Some("io")),
        _ => (Verdict::Ok, None),
    };
    let done = resolve(ws, dirs, &caller.env, SystemTime::now()).and_then(|r| match name {
        "install" => {
            let names: Vec<String> = args.target.iter().chain(&args.rest).cloned().collect();
            let (rows, todo) = install(&r, d, dirs, ws, caller, &names, cancel)?;
            let (verdict, err) = match verdict(&rows) {
                (Verdict::Ok, _) if rows.iter().any(|s| s.state != "ready") => (Verdict::Unready, None),
                v => v,
            };
            let counts = format!("{} of {todo} ready", rows.iter().filter(|s| s.state == "ready").count());
            Ok(Outcome { states: rows, err, ..Outcome::new("install", verdict, root.clone(), counts) })
        }
        "lock" => {
            let target = args.flags.get("target").and_then(Value::as_str).unwrap_or(target());
            let (rows, path) = pins::refresh(d, dirs, ws, &r, args.target.as_deref(), target, args.flags.contains_key("refresh"))?;
            let ((verdict, err), failed) = (verdict(&rows), rows.iter().filter(|s| s.actor.is_some()).count());
            let counts = format!("{} looked up for {target}, {failed} failed; nothing installed", rows.len());
            Ok(Outcome { states: rows, err, notes: vec![format!("lock: {}", path.display())], ..Outcome::new("lock", verdict, root.clone(), counts) })
        }
        _ => store::gc(dirs).map(|gone| Outcome { notes: gone.clone(), ..Outcome::new("gc", Verdict::Ok, root.clone(), format!("{} removed", gone.len())) }),
    });
    done.unwrap_or_else(|e| Outcome::error(name, if cancel.is_cancelled() { "cancel" } else { "io" }, format!("{e:#}")))
}

/// `itse install [pkg…]`: every package in use that is pinned for this target and missing, or the
/// named ones, then a shim for each of their bins, linked to the calling CLI (`exe_path`). A
/// package that fails (its shim included) is its own row; the others go on. Cancel is looked at
/// between packages. Returns the rows and how many packages were asked for.
fn install(r: &Resolved, d: &dyn Delegate, dirs: &Dirs, ws: &Workspace, caller: &Caller, names: &[String], cancel: &Cancel) -> Result<(Vec<State>, usize)> {
    let (lock, packages) = (lock::Lock::load(dirs, ws, r.trusted)?.0, dirs.data.join("store"));
    let present = r.roots.iter().filter(|(_, root)| root.starts_with(&packages)).map(|(n, _)| n.clone());
    let missing = r.states.iter().filter_map(|s| s.next.as_ref().filter(|n| n.len() == 2 && n[0] == "install").map(|n| n[1].clone()));
    let todo: BTreeSet<String> = present.chain(missing).filter(|n| names.is_empty() || names.contains(n)).collect();
    let mut rows = Vec::new();
    for name in &todo {
        ensure!(!cancel.is_cancelled(), "cancelled after {} of {} packages", rows.len(), todo.len());
        let item = store::row_of(&r.states, name).map_or(name.clone(), |s| s.item.clone());
        let (Some(pkg), Some(pin)) = (r.manifest.package(name), lock.packages.get(name).and_then(|l| l.targets.get(target()))) else { continue };
        let linked = store::install(d, dirs, pkg, pin).and_then(|dir| Ok(env::link_shims(dirs, pkg.bin.keys(), &caller.exe_path).map(|()| dir)?));
        rows.push(match linked {
            Ok(dir) => State { item, state: "ready".into(), reason: Some(format!("store {}", dir.display())), ..State::default() },
            Err(f) => State { hosts: pkg.hosts.clone(), ..f.row(&item) },
        });
    }
    for name in names.iter().filter(|n| !todo.contains(*n)) {
        let missing = || State { item: name.clone(), state: "missing".into(), reason: Some("no such package in use".into()), ..State::default() };
        rows.push(store::row_of(&r.states, name).cloned().unwrap_or_else(missing));
    }
    Ok((rows, todo.len().max(names.len())))
}

/// A `Local` verb run in the CLI process (`verbs`, `use`, `unuse`, `integrate`), recorded through a
/// short-lived writer; its text and exit code. `tty` says whether stdin is a terminal.
pub fn local(usage: &Usage, ws: &Workspace, dirs: &Dirs, verb: &Verb, args: &Args, tty: bool) -> (String, u8) {
    let (started, env): (_, BTreeMap<String, String>) = (Instant::now(), std::env::vars().collect());
    let caller = Caller { id: usage.ident("cli", &env), mode: "cli".into(), env, ..Caller::default() };
    let answer = if verb.name == "verbs" {
        // `debug` is listed where an extension in use declares how its tests run under a debugger.
        let r = resolve(ws, dirs, &caller.env, SystemTime::now());
        let debug = r.is_ok_and(|r| r.manifest.uses().iter().filter_map(|u| r.manifest.exts.get(&u.id)).any(|e| e.0.debug.as_ref().is_some_and(|d| !d.adapter.is_empty())));
        let text = index(args.flags.contains_key("json"), debug);
        Answer { verdict: Verdict::Ok, full: text.len() as u64, text, ..Answer::default() }
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut o = match verb.name {
            "integrate" => harness::integrate(ws, dirs, &caller.env, &cwd, args, tty).unwrap_or_else(|e| Outcome::error("integrate", "bad_args", format!("{e:#}"))),
            _ => manifest_verb(ws, dirs, &caller.env, &cwd, verb.name, args, tty),
        };
        let text = o.render("cli", 1, VERB, false).unwrap_or_else(|line| line + "\n");
        Answer { verdict: o.verdict, full: o.full.bytes, trunc: o.page.of > 1, err: o.err, text, ..Answer::default() }
    };
    record(usage, &caller, verb.name, None, started.elapsed().as_millis() as u64, &answer);
    (answer.text.clone(), exit(&answer))
}

/// `itse verbs [--json]`: the verb index for agents, from the table, without human-only and
/// internal verbs, and with `debug` only if `debug`. Declared actions and extension skills are
/// not listed yet.
pub fn index(json: bool, debug: bool) -> String {
    let rows: Vec<(&str, String, &str, &str)> = VERBS
        .iter()
        .filter(|v| !v.human_only && !v.name.starts_with("__") && (debug || v.tool != Tool::Debug))
        .map(|v| (v.name, synopsis(v), crate::mcp::tool_name(v.tool), v.about))
        .collect();
    if json {
        let verbs: Vec<Value> = rows.iter().map(|(n, a, t, about)| json!({"name": n, "args": a, "tool": t, "about": about})).collect();
        return json!({"verbs": verbs, "actions": [], "skills": []}).to_string() + "\n";
    }
    let tail = "exit codes: 0 ok, 1 the verb reports failure, 2 error, 3 needs a human, 4 running, 130 interrupted\nresults: the first line is VERDICT verb scope — counts; ids such as run:14/T1 open with 'itse show'\nactions: none declared\n";
    rows.iter().map(|(n, a, _, about)| format!("itse {n} {a}").trim_end().to_string() + &format!("\n    {about}\n")).collect::<String>() + tail
}

/// A verb's arguments as the index and the skill show them: `<name> [--kind V]`.
pub fn synopsis(v: &Verb) -> String {
    let value = |n: &str| match n {
        "at" => "path:line",
        "log" => "path:line=text",
        "frame" => "F",
        _ => "V",
    };
    let arg = |p: &Param| match p {
        Req(n) => format!("<{n}>"),
        Pos(n) => format!("[{n}]"),
        Rest(n) => format!("[{n}…]"),
        Flag(n) => format!("[--{n}]"),
        Opt(n) => format!("[--{n} {}]", value(n)),
        Many(n) => format!("[--{n} {}…]", value(n)),
    };
    v.params.iter().map(arg).collect::<Vec<_>>().join(" ")
}

/// `status`: a row per item from the resolver, `suggested` ones included, then the
/// workspace, its repo and the daemon; with `--hosts`, the hosts the missing packages need.
fn status(host: &Host, caller: &Caller, args: &Args, cancel: &Cancel) -> Outcome {
    // A debug-only seam for the Ctrl-C test: a status that takes this long unless cancelled.
    let slow = caller.env.get("ITSE_TEST_SLOW_MS").filter(|_| cfg!(debug_assertions)).and_then(|ms| ms.parse().ok());
    if slow.is_some_and(|ms| sys::wait_until(Duration::from_millis(ms), || cancel.is_cancelled())) {
        return Outcome::error("status", "cancel", text("cancelled", &[]));
    }
    let r = match resolve(&host.ws, &host.dirs, &caller.env, SystemTime::now()) {
        Ok(r) => r,
        Err(e) => return Outcome::error("status", "bad_args", format!("{e:#}")),
    };
    let (ws, pid, up) = (&host.ws, process::id(), host.started.elapsed().unwrap_or_default().as_secs());
    let (root, name) = (ws.root.display(), ws.repo_name().display());
    let common = if ws.git { format!("; git common dir {}", ws.repo.display()) } else { String::new() };
    let git = if ws.git { "" } else { "not a git work tree; " };
    let (debug, bytes) = (host.config().mcp.debug, crate::mcp::list_bytes);
    let listing = crate::mcp::client::listing(host, &caller.env, debug);
    let (n, b, cap) = (listing.tools.len(), listing.bytes, crate::mcp::LIST_CAP);
    let on = if debug { "on".into() } else { format!("off (mcp.debug: true adds ~{} tokens)", (bytes(true) - bytes(false)) * 10 / 36) };
    let tools = format!("mcp tools: {n} (~{} tokens incl. instructions; list {b} of {cap} B); debug tool {on}", b * 10 / 36);
    let log = host.usage.path();
    let usage = if host.usage.on(&caller.env) { format!("on, {} ({:.1} MB)", log.display(), fs::metadata(&log).map_or(0, |m| m.len()) as f64 / 1e6) } else { "off".into() };
    let mut notes = vec![
        format!("workspace: {root} ({git}ws {})", ws.ws),
        format!("repo: {name} (rid {}{common})", ws.rid),
        format!("daemon: pid {pid}, started by {} {up} s ago; {VERSION}; proto {}; env keys {}", host.started_by, proto(), host.env_keys_sha()),
        format!("socket: {}", host.socket.display()),
        tools,
    ];
    notes.extend(listing.notes);
    notes.extend([format!("usage log: {usage}; ITSE_USAGE=off or \"usage\": false in {} stops it", host.dirs.config.join("config.json").display())]);
    notes.extend(r.notes);
    if args.flags.contains_key("hosts") {
        let rows: Vec<String> = r.states.iter().filter(|s| !s.hosts.is_empty()).map(|s| format!("{} from {}", s.item, s.hosts.join(", "))).collect();
        notes.push(format!("hosts: {}", if rows.is_empty() { "none; nothing in use waits for a fetch".into() } else { rows.join("; ") }));
    }
    let count = |f: fn(&str) -> bool| r.states.iter().filter(|s| f(&s.state)).count();
    let (unready, suggested) = (count(|s| !matches!(s, "ready" | "unpinned" | "expiring" | "suggested")), count(|s| s == "suggested"));
    let counts = [(unready > 0).then(|| format!("{unready} of {} not ready", r.states.len() - suggested)), (suggested > 0).then(|| format!("{suggested} suggested"))];
    let counts = counts.into_iter().flatten().chain([format!("daemon pid {pid}")]).collect::<Vec<_>>().join("; ");
    let verdict = if unready > 0 { Verdict::Unready } else { Verdict::Ok };
    let servers = r.launches.iter().filter_map(|l| match l.protocol {
        crate::host::Protocol::Lsp => Some(crate::lsp::row(host, l)),
        crate::host::Protocol::Mcp => Some(host.row("mcp", l, |_| Vec::new())),
        crate::host::Protocol::Dap => None,
    });
    let debug = crate::dap::rows(host, &r.launches);
    Outcome { states: r.states.iter().cloned().chain(servers).chain(debug).collect(), notes, ..Outcome::new("status", verdict, root.to_string(), counts) }
}

// --- daemon status and stop (CLI only) ---

/// `itse daemon status`: the daemon's row, from `daemon.json`. It never starts a daemon.
pub fn daemon_status(ws: &Workspace, dirs: &Dirs) -> String {
    let Some((pid, _, info)) = client::running(ws, dirs) else { return format!("no daemon runs for {}", ws.root.display()) };
    let s = |k: &str| info[k].as_str().unwrap_or("?").to_string();
    format!("daemon pid {pid} {} started {} by {} (env keys {})", s("version"), s("started_at"), s("started_by"), s("env_keys_sha"))
}

/// `itse daemon stop`: ask the daemon to stop, wait for it, and kill its group if it doesn't go.
/// It never starts a daemon; one that answers without a live `daemon.json` is only asked.
pub fn stop_daemon(ws: &Workspace, dirs: &Dirs) -> Result<String> {
    let (live, sock) = (client::running(ws, dirs), ws.socket(dirs, Path::new("/tmp"))?);
    let told = UnixStream::connect(sock).is_ok_and(|s| client::ask(&mut BufReader::new(s), "shutdown", json!({"reason": "stop"}), Some(Duration::from_secs(2))).is_ok());
    let Some((pid, start, _)) = live else { return Ok(format!("{} for {}", if told { "told the daemon to stop" } else { "no daemon runs" }, ws.root.display())) };
    let stopped = sys::wait_until(Duration::from_secs(4), || !sys::alive(pid, start));
    if !stopped {
        sys::kill_group(pid, Duration::from_secs(1), || ());
    }
    Ok(if stopped { format!("stopped daemon pid {pid}") } else { format!("killed daemon pid {pid}; it didn't stop in 4 s") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(verb: &str, tokens: &[&str]) -> Result<Args> {
        parse_args(find(verb).unwrap(), tokens.iter().map(OsString::from))
    }

    #[test]
    fn every_verb_name_is_unique_and_every_row_fits_its_tool() {
        for (i, v) in VERBS.iter().enumerate() {
            assert!(VERBS[..i].iter().all(|w| w.name != v.name), "{} twice", v.name);
            assert!(v.tool == Tool::CliOnly || v.runs == Daemon || v.name == "use", "{} is served by an MCP tool, so the daemon runs it", v.name);
        }
        assert!(VERBS.iter().filter(|v| v.human_only).all(|v| v.tool == Tool::CliOnly));
    }

    #[test]
    fn verbs_json_lists_every_verb_in_the_table() {
        let index_of = |debug: bool| serde_json::from_str::<Value>(&index(true, debug)).unwrap();
        let (all, plain) = (index_of(true), index(false, true));
        assert!(!index_of(false)["verbs"].as_array().unwrap().iter().any(|v| v["name"] == "debug"), "debug only where a debug adapter is declared");
        for v in VERBS {
            // Left out: human-only verbs (an agent can't run them) and internal ones.
            let left_out = v.human_only || v.name.starts_with("__");
            let row = all["verbs"].as_array().unwrap().iter().find(|r| r["name"] == v.name);
            assert_eq!(row.is_none(), left_out, "{}: {all}", v.name);
            if let Some(row) = row {
                assert_eq!(row["about"], v.about, "{row}");
                assert!(plain.contains(&format!("\n    {}\n", v.about)), "{}: {plain}", v.name);
            }
        }
    }

    #[test]
    fn args_split_into_target_rest_and_flags() {
        let args = parsed("test", &[":data", "OrderRepositoryTest", "--action", "gradle:test"]).unwrap();
        assert_eq!(args.target.as_deref(), Some(":data"));
        assert_eq!(args.rest, ["OrderRepositoryTest"]);
        assert_eq!(args.flags["action"], "gradle:test");
        let args = parsed("problems", &["data/", "--all", "--new"]).unwrap();
        assert_eq!((args.flags["all"].clone(), args.flags["new"].clone()), (json!(true), json!(true)));
        let args = parsed("run", &["dev", "--input", "a=1", "--input=b=2", "--", "--port", "8080"]).unwrap();
        assert_eq!((args.flags["input"].clone(), args.rest), (json!(["a=1", "b=2"]), vec!["--port".to_string(), "8080".into()]));
        assert_eq!(parsed("show", &["run:14", "--page=2"]).unwrap().flags["page"], "2");
    }

    #[test]
    fn unknown_flags_extra_values_and_flag_values_are_errors_in_lowercase() {
        assert_eq!(parsed("status", &["--frobnicate"]).unwrap_err().to_string(), "invalid option '--frobnicate'");
        assert_eq!(parsed("status", &["extra"]).unwrap_err().to_string(), "unexpected argument \"extra\"");
        assert_eq!(parsed("info", &["a", "b"]).unwrap_err().to_string(), "unexpected argument \"b\"");
        assert!(parsed("problems", &["--all=yes"]).is_err());
        assert!(parsed("find", &["x", "--kind"]).unwrap_err().to_string().contains("missing argument"));
    }

    #[test]
    fn args_round_trip_as_the_call_json() {
        let args = parsed("lock", &["--refresh", "kotlin-lsp"]).unwrap();
        let v = serde_json::to_value(&args).unwrap();
        assert_eq!(v, json!({"target": "kotlin-lsp", "rest": [], "flags": {"refresh": true}}));
        assert_eq!(serde_json::from_value::<Args>(v).unwrap(), args);
    }
}
