//! itse-bench — the with/without-itse baseline for any coding-agent harness: Harbor-shaped tasks, a harness adapter,
//! both arms per rep, one results.jsonl line per trial. Unit tests live under tests/unit/ (`#[path]`).

mod adapter;
mod report;
mod run;
mod task;
mod toml;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail, ensure};
use lexopt::prelude::*;

const USAGE: &str = "usage: itse-bench run --tasks DIR --harness NAME --out DIR [--adapters FILE] [--reps N] [--seed N]
                      [--itse BIN] [--model M] [--permission-mode MODE] [--max-turns N] [--trials DIR]
       itse-bench report DIR|FILE...
       itse-bench check-task DIR...

run         each task x rep in both arms, in shuffled order; a results.jsonl line per trial in --out. Defaults: the
            crate's adapters.toml, 3 reps, the clock as seed, the itse on PATH (masked from both arms), sonnet,
            bypassPermissions, --max-turns over the task's max_turns over 40, trials in $TMPDIR/itse-bench
report      pass rates, the paired table, the sign test and medians, per harness
check-task  load and validate task dirs (or dirs of tasks); prints each task's sha256
";

fn main() -> ExitCode {
    cli().unwrap_or_else(|e| {
        eprintln!("itse-bench: {e:#}");
        ExitCode::from(if run::stopped() { 130 } else { 2 })
    })
}

fn cli() -> Result<ExitCode> {
    let mut p = lexopt::Parser::from_env();
    let cmd = match p.next()? {
        Some(Value(v)) => Some(v.string()?),
        Some(Long("help") | Short('h')) | None => None,
        Some(arg) => return Err(arg.unexpected().into()),
    };
    let rest: Vec<OsString> = p.raw_args()?.collect();
    let Some(cmd) = cmd.filter(|_| !rest.iter().any(|a| a == "-h" || a == "--help")) else {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    };
    let paths: Vec<PathBuf> = rest.iter().map(PathBuf::from).collect();
    match cmd.as_str() {
        "run" if env::var_os(run::WORKER).is_none() => run::supervise(),
        "run" => run::run(&run_opts(lexopt::Parser::from_args(rest))?).map(|()| ExitCode::SUCCESS),
        "report" | "check-task" if paths.is_empty() => bail!("{cmd} needs a path\n{USAGE}"),
        "report" => report::report(&paths).map(|text| print!("{text}")).map(|()| ExitCode::SUCCESS),
        "check-task" => {
            let dirs = paths.iter().map(|d| task::list(d)).collect::<Result<Vec<_>>>()?.concat();
            let lines: Vec<Result<String>> = dirs.iter().map(|d| task::Task::load(d).map(|t| format!("ok {} sha256:{} tags {}", t.id, t.sha, t.tags.join(",")))).collect();
            lines.iter().for_each(|l| println!("{}", l.as_ref().map_or_else(|e| format!("error {e:#}"), String::clone)));
            Ok(if lines.iter().all(Result::is_ok) { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        other => bail!("unknown command '{other}'; itse-bench --help lists them"),
    }
}

fn run_opts(mut p: lexopt::Parser) -> Result<run::Opts> {
    let mut o = run::Opts {
        tasks: PathBuf::new(),
        harness: String::new(),
        adapters: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/adapters.toml")),
        reps: 3,
        seed: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        out: PathBuf::new(),
        itse: None,
        model: "sonnet".into(),
        permission_mode: "bypassPermissions".into(),
        max_turns: None,
        root: env::current_dir()?.join(env::temp_dir()).join("itse-bench"),
    };
    while let Some(arg) = p.next()? {
        match arg {
            Long("tasks") => o.tasks = p.value()?.into(),
            Long("harness") => o.harness = p.value()?.string()?,
            Long("out") => o.out = p.value()?.into(),
            Long("adapters") => o.adapters = p.value()?.into(),
            Long("reps") => o.reps = num(&mut p, "--reps")?,
            Long("seed") => o.seed = num(&mut p, "--seed")?,
            Long("itse") => o.itse = Some(p.value()?.into()),
            Long("model") => o.model = p.value()?.string()?,
            Long("permission-mode") => o.permission_mode = p.value()?.string()?,
            Long("max-turns") => o.max_turns = Some(num(&mut p, "--max-turns")?),
            Long("trials") => o.root = env::current_dir()?.join(p.value()?),
            arg => return Err(arg.unexpected().into()),
        }
    }
    ensure!(!o.tasks.as_os_str().is_empty() && !o.harness.is_empty() && !o.out.as_os_str().is_empty(), "run needs --tasks, --harness and --out\n{USAGE}");
    ensure!(o.reps > 0 && o.max_turns != Some(0), "--reps and --max-turns must be above 0");
    Ok(o)
}

fn num<T: std::str::FromStr<Err: std::fmt::Display>>(p: &mut lexopt::Parser, name: &str) -> Result<T> {
    let v = p.value()?.string()?;
    v.parse().map_err(|e| anyhow!("{name} {v:?}: {e}"))
}
