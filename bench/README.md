# itse-bench

The with/without-itse baseline for coding-agent harnesses. It runs each task through a harness twice per rep, once with itse and once without, in fresh per-trial dirs, and reports pass rates, a paired table with an exact sign test, and medians of wall time, tokens and cost. It is a separate crate, never linked into itse; it installs no agents, runs no containers and downloads nothing.

```sh
cargo build --manifest-path bench/Cargo.toml
B=bench/target/debug/itse-bench
$B check-task bench/tasks
$B run --tasks bench/tasks --harness claude-code --reps 3 --seed 1 --out /tmp/bench-1 --itse target/debug/itse --model sonnet
$B report /tmp/bench-1
```

## Tasks

A task is a dir in Harbor's layout, with `environment/setup.sh` in place of a Dockerfile:

| File | What it is |
|---|---|
| `instruction.md` | what the agent is told, verbatim |
| `task.toml` | `version`; `[metadata]` with `tags` (`control` marks a task where itse should not help); `[agent]` and `[verifier]` with `timeout_sec` (seconds, an integer or a float); `[itse_bench]` with `use` (the extensions the `with` arm declares) and `max_turns` |
| `environment/setup.sh` | `setup.sh WS` creates the repo in WS and commits the base |
| `tests/test.sh` | the verifier, run from the task dir with `WORKSPACE` set; exit 0 is a pass. `rust-off-by-one`'s also fails on any change outside `src/`, so a `.cargo/config.toml` or `build.rs` cannot steer `cargo test` |
| `solution/solve.sh` | the reference solution, run in the workspace; it makes `tests/test.sh` pass |

Other Harbor keys are allowed and ignored. Unlike Harbor, the verdict is the exit status of `tests/test.sh`, not `/logs/verifier/reward.txt`, and paths come from `$WORKSPACE` and the task dir, not `/app` or `/tests`; a Harbor task must be adapted before use, and `check-task` refuses a verifier that writes to `/logs/verifier`. `--max-turns` wins over a task's `max_turns`, which wins over 40. `check-task` validates a task and prints its sha256, taken over the task's files sorted by path; every results line carries it. In a dir of tasks, every subdir that holds any task file is a task, so one whose `task.toml` is missing or misnamed is reported, not skipped. The starter tasks are `rust-off-by-one` (a failing test, with a hidden test in the verifier) and `rust-doc-control` (docs only, a control).

## Trials

Each trial lives in `$TMPDIR/itse-bench/<run>/<task>-<rep>-<arm>/` (`/tmp` when `TMPDIR` is unset; `--trials DIR` moves it) with `ws/` (the repo), `state/`, `cfg/` and `data/` (the trial's `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME`; `cfg/` is also its `HOME`, and the harness's fresh config goes under it), and `bin/`, which holds only an `itse` link. Harnesses read instruction files in every dir above their workspace, so the run refuses to start when the trial root or a dir above it holds `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md` or `.claude/`. The arms differ in PATH alone:

- `with`: `bin/` and the trial's shim dir come first. Before the harness starts, the bench runs `itse use <ext>` for each `use` entry and `itse trust`, and `itse integrate <harness> --skill --home cfg/` once itse has it (so the harness's `~` is `cfg/`). The agent is never told about itse.
- `without`: the bench's own PATH, with every dir that offers itse (the binary, or a shim linked to it) swapped for a run-level `tools/<n>/` dir that links that dir's other entries, in the dir's place on PATH, so every tool resolves as it does on the bench's PATH. So `itse` on PATH (the default for `--itse`) or in `$CARGO_HOME/bin` is fine: cargo and the rest are the same in both arms, and the bench checks that the `without` arm finds no itse and a cargo that is not itse.

Before the first trial, each task's `use` entries are tried in a scratch dir, so a misspelt extension stops the run at once. Trials see only an allowlist of the bench's env (PATH, USER, LANG and `LC_*`, TERM, SHELL, TMPDIR, the proxy and CA settings, and the Rust toolchain's homes, which default to `~/.cargo` and `~/.rustup` of the bench's HOME when those exist) plus what the adapter sets or names in `pass_env`, with HOME set to `cfg/`, so the user's dotfiles never apply; other secrets such as `GH_TOKEN` never reach the agent. git reads only the trial's own `gitconfig` (a fixed name and email; `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM=1`), in setup, the harness and the verifier alike, so signing, templates and hooks from the user's config never apply. Each run gets a fresh dir named after the time, the seed and the bench's pid.

The order of the arms is shuffled per rep from `--seed` (xorshift64) and recorded. The harness runs in `ws/` in its own process group, with SIGTERM at `[agent] timeout_sec` and SIGKILL 5 s later. Afterwards the bench reads the daemon's `daemon.json` and peak RSS, runs `itse daemon stop`, and kills any process that still carries the trial's `XDG_STATE_HOME` after 5 s (a `tail -f` on a trial log is left alone); it does so even when setup or the harness fails. Then it runs the verifier, records `git diff --shortstat` against the base and the number of new untracked files (with the trial's env, a time limit, and none of the repo's hooks, fsmonitor, diff drivers or filter drivers: it writes `* -filter` to the repo's `.git/info/attributes` first), the trial's own usage log, the turn cap, permission mode and agent timeout used, and the pids it had to kill (`leftovers`), and appends a line to `<out>/results.jsonl`. Leftovers do not stop the run. Each trial prints `pass`, `fail (timed out)`, `fail (verifier exit N)` or `harness error (exit N; ...)`.

`itse-bench run` does its work in a worker process in a group of its own, so Ctrl-C (or SIGTERM or SIGHUP to the process you started) never lands in a harness: the worker sees it, ends the trial's group as on a timeout, stops the daemon, kills its leftovers and exits 130, with no line recorded for the trial.

The agent runs unsandboxed, so it can read and write the task dir, including `tests/` and `solution/`. The bench hashes the task dir again before and after the verifier and stops the run, with no line recorded, if it changed.

## Adapters

`adapters.toml` declares each harness: `version_cmd`, `command` (with `{instruction}`, `{mcp_config}`, `{model}`, `{permission_mode}`, `{max_turns}`, `{ws}`, `{cfg}`, `{arm}`, `{task}`, `{task_dir}`, `{trial}`, `{adapter_dir}`), `parser` (`json-last` or `none`), `env`, `pass_env`, `require_env` (the run refuses to start while one is unset), `fields` (JSON pointers into the last JSON line of stdout for `fresh_tokens`, `cache_read_tokens`, `cache_write_tokens`, `output_tokens`, `cost_usd`, `turns` and `is_error`; a missing one is null) and `arms.with` / `arms.without`.

The `claude-code` adapter runs `claude -p` with `--strict-mcp-config`, an MCP config per arm, and a turn cap. It runs on a subscription: there is no API key and no dollar cap, and each trial gets a fresh `CLAUDE_CONFIG_DIR` at `cfg/.claude`, which is where `itse integrate --home cfg/` puts the skill. A fresh config holds no login, so `CLAUDE_CODE_OAUTH_TOKEN` must be set in the env that runs itse-bench (`claude setup-token` makes one, or a gateway provides it with `ANTHROPIC_BASE_URL`). Claude Code does not pass it on to the commands it runs, so start itse-bench from a terminal, not from inside a Claude Code session. Only those two pass through; no credentials are copied. Every trial sets `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` and `DISABLE_AUTOUPDATER=1`, so trials stay alike whatever the bench's env holds. The self-tests use the `fake` adapter in `tests/fixtures/`, which spends nothing.

## Reading a report

Tokens and cost are compared only within one harness; each harness gets its own section. The token medians are input (fresh plus cache-write) and output; cache reads, which dwarf both, stay in `results.jsonl` only. A trial whose harness exited non-zero (or reported `is_error`) with no tokens and no timeout is a harness error, such as a failed login, which Claude Code reports as one turn with no tokens: it is counted among the trials and left out of pass rates and pairs. Cost (`est$`) is the harness's own price estimate; a subscription trial spends no dollars. Control tasks are marked `(control)`, and their pairs get their own sign test, apart from the rest. A task whose `task_sha` differs between results is shown as one row per sha (`id@sha`). Pairs are the two arms of one rep in one run, so runs appended to one `results.jsonl` (or several files) all count; a trial that appears twice is an error. 10 tasks × 3 reps detect only large effects, so a p above 0.05 says little.

## Development

`cargo test --manifest-path bench/Cargo.toml` runs the self-tests; none starts a real harness. Unit tests live in `tests/unit/<module>.rs`, loaded with `#[path]`, so `src/` holds only the code; end-to-end tests in `tests/e2e.rs` drive the built binary against the built itse.
