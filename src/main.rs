//! itse — a small workbench for coding agents and their harnesses.
//!
//! It reads a workspace manifest, installs the declared language servers and tools into user
//! space from sha256 pins, keeps those servers warm in one daemon per workspace, and answers a
//! small set of verbs with compact, located, paged results through the CLI and one MCP endpoint.
//! It decides nothing for the agent: it starts nothing that isn't declared.
//!
//! The CLI parses with lexopt against the verb table (`verbs`), runs `Local` verbs itself, and
//! sends every other verb to the workspace daemon (`daemon`), starting it when none runs.

mod actions;
mod daemon;
mod env;
mod host;
mod lock;
mod lsp;
mod manifest;
mod mcp;
mod parse;
mod pins;
mod resolve;
mod results;
mod rpc;
mod store;
mod sys;
mod usage;
mod verbs;
mod workspace;

use std::env::{args_os, current_dir, var_os};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use lexopt::prelude::*;
use serde_json::json;

use daemon::client;
use verbs::{Args, Runs, Verb};
use workspace::{Dirs, Workspace};

const HELP_TAIL: &str = "
  --root DIR  the workspace root (else ITSE_WORKSPACE, else the nearest folder with .git)
  --version   the version

exit codes: 0 ok, 1 the verb reports failure, 2 error, 3 needs a human, 4 running, 130 interrupted
";

#[derive(Debug)]
enum Cmd {
    Version,
    Help,
    Verb(&'static Verb, Args),
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("itse: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    sys::refuse_test_seams()?;
    // A shim is a link to itse named after the tool it runs.
    // The daemon and runners start as /proc/self/exe.
    let argv0 = PathBuf::from(args_os().next().unwrap_or_default());
    if let Some(tool) = argv0.file_name().and_then(|n| n.to_str()).filter(|n| !n.starts_with("itse") && argv0 != Path::new("/proc/self/exe")) {
        return env::shim(tool, args_os().skip(1).collect());
    }
    let (root, cmd) = parse(args_os().skip(1))?;
    let (verb, args) = match cmd {
        Cmd::Version => return Ok(print(format!("itse {}\n", env!("CARGO_PKG_VERSION")), 0)),
        Cmd::Help => return Ok(print(help(), 0)),
        Cmd::Verb(verb, args) if verb.name == "__run" => return actions::runner(Path::new(args.target.as_deref().unwrap_or_default())),
        // Its workspace is the edited file's, and it always exits 0.
        Cmd::Verb(verb, args) if verb.name == "hook" => return Ok(verbs::harness::hook(args.target.as_deref().unwrap_or_default())),
        Cmd::Verb(verb, args) => (verb, args),
    };
    let given = root.or_else(|| var_os("ITSE_WORKSPACE").filter(|v| !v.is_empty()).map(PathBuf::from));
    let ws = Workspace::find(given, &current_dir()?)?;
    let dirs = Dirs::from_env(|k| var_os(k))?;
    match (verb.name, args.target.as_deref()) {
        ("__daemonize", _) => daemonize(&ws, &dirs)?,
        ("daemon", None) => daemon::run(ws, dirs)?,
        ("daemon", Some("stop")) => println!("{}", verbs::stop_daemon(&ws, &dirs)?),
        ("daemon", Some("status")) => println!("{}", verbs::daemon_status(&ws, &dirs)),
        ("daemon", Some("restart")) => {
            verbs::stop_daemon(&ws, &dirs)?;
            println!("daemon pid {} started", client::connect(&ws, &dirs, "cli", &[])?.1["pid"]);
        }
        ("daemon", Some(other)) => bail!("unexpected argument '{other}'; 'daemon' takes stop, restart or status"),
        ("mcp", _) => return mcp::relay::run(&ws, &dirs),
        ("env" | "exec", _) => return env::cli(&ws, &dirs, &args, verb.name == "exec"),
        ("stats", _) => return Ok(print(usage::stats(&dirs, &args, &mcp::offered())?, 0)),
        ("verbs" | "use" | "unuse" | "integrate", _) => {
            let (text, exit) = verbs::local(&usage::Usage::new(&dirs, &ws.root, false), &ws, &dirs, verb, &args, io::stdin().is_terminal());
            return Ok(print(text, exit));
        }
        (name, _) if verb.runs == Runs::Local => bail!("'{name}' is not built yet"),
        (name, _) => {
            let conn = client::connect(&ws, &dirs, "cli", &[])?;
            let (text, exit) = client::call(conn, json!({"verb": name, "args": args, "json": false, "page": 1, "wait": 100, "tty": io::stdin().is_terminal()}))?;
            return Ok(print(text, exit));
        }
    }
    Ok(0)
}

/// `itse __daemonize`: start the daemon in its own session, with its output in `daemon.log`, and
/// return at once. The CLI reaps this process, so the daemon is never its child or its zombie.
fn daemonize(ws: &Workspace, dirs: &Dirs) -> Result<()> {
    let state = ws.state_dir(dirs);
    sys::private_dir(&state)?;
    let log = OpenOptions::new().create(true).append(true).mode(0o600).open(state.join("daemon.log"))?;
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("--root").arg(&ws.root).arg("daemon").current_dir(&ws.root).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: new_session only calls setsid, which is async-signal-safe.
    unsafe { cmd.pre_exec(sys::new_session) };
    cmd.spawn().context("starting the daemon")?;
    Ok(())
}

fn print(text: String, exit: u8) -> u8 {
    print!("{text}");
    exit
}

fn help() -> String {
    let mut out = String::from("itse — a workbench for coding agents and their harnesses\n\nusage: itse [--root DIR] <verb> [args]\n\n");
    for v in verbs::VERBS.iter().filter(|v| !v.name.starts_with("__")) {
        out += &format!("  {:<10}  {}\n", v.name, v.about);
    }
    out + HELP_TAIL
}

/// `--root` belongs to the CLI, wherever it stands before `--`; the rest after the verb is the
/// verb's, parsed by the same function as MCP's `args`.
fn parse(args: impl IntoIterator<Item = OsString>) -> Result<(Option<PathBuf>, Cmd)> {
    let mut args: Vec<OsString> = args.into_iter().collect();
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut root = None;
    if let Some(i) = args[..end].iter().position(|a| a == "--root" || a.to_str().is_some_and(|a| a.starts_with("--root="))) {
        let flag = args.remove(i);
        match flag.to_str().and_then(|f| f.strip_prefix("--root=")) {
            Some(dir) => root = Some(PathBuf::from(dir)),
            None if i < args.len() => root = Some(PathBuf::from(args.remove(i))),
            None => bail!("missing argument for option '--root'"),
        }
    }
    let mut parser = lexopt::Parser::from_args(args);
    let cmd = match parser.next()? {
        None => Cmd::Help,
        Some(Long("version") | Short('V')) => Cmd::Version,
        Some(Long("help") | Short('h')) => Cmd::Help,
        Some(Value(v)) if v == "help" => Cmd::Help,
        Some(Value(v)) => {
            let name = v.string()?;
            let verb = verbs::find(&name).ok_or_else(|| anyhow!("unknown verb '{name}'; 'itse help' lists the verbs"))?;
            Cmd::Verb(verb, verbs::parse_args(verb, parser.raw_args()?)?)
        }
        Some(arg) => return Err(arg.unexpected().into()),
    };
    Ok((root, cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<(Option<PathBuf>, String)> {
        parse(args.iter().map(OsString::from)).map(|(root, cmd)| (root, format!("{cmd:?}")))
    }

    fn verb(args: &[&str]) -> (Option<PathBuf>, &'static str, Args) {
        match parse(args.iter().map(OsString::from)).unwrap() {
            (root, Cmd::Verb(v, a)) => (root, v.name, a),
            (_, cmd) => panic!("{cmd:?}"),
        }
    }

    #[test]
    fn verbs_and_flags_parse() {
        assert_eq!(parsed(&[]).unwrap(), (None, "Help".into()));
        assert_eq!(parsed(&["--version"]).unwrap().1, "Version");
        assert_eq!(parsed(&["help"]).unwrap().1, "Help");
        assert_eq!(verb(&["--root", "/w", "status"]).0, Some(PathBuf::from("/w")));
        assert_eq!(verb(&["status", "--root=/w", "--hosts"]).0, Some(PathBuf::from("/w")));
        let (root, name, args) = verb(&["run", "dev", "--", "--root", "x"]);
        assert_eq!((root, name, args.rest), (None, "run", vec!["--root".to_string(), "x".into()]), "after -- it is the action's");
        assert_eq!(verb(&["daemon", "stop"]).2.target.as_deref(), Some("stop"));
    }

    #[test]
    fn unknown_verbs_and_flags_are_errors_in_lowercase() {
        assert_eq!(parsed(&["tset"]).unwrap_err().to_string(), "unknown verb 'tset'; 'itse help' lists the verbs");
        assert_eq!(parsed(&["status", "--frobnicate"]).unwrap_err().to_string(), "invalid option '--frobnicate'");
        assert!(parsed(&["status", "extra"]).is_err());
        assert_eq!(parsed(&["status", "--root"]).unwrap_err().to_string(), "missing argument for option '--root'");
    }

    #[test]
    fn help_lists_every_public_verb_once() {
        let text = help();
        assert!(text.contains("  status      the workspace, its items and their states\n"), "{text}");
        assert!(!text.contains("__daemonize") && !text.contains("rename"));
    }
}
