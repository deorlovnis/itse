# itse

A small workbench for coding agents and their harnesses, inside a Halti box. Itse reads a workspace manifest (suggesting entries from files it detects, never enabling them by itself), installs the declared language servers, debug adapters and tools into user space from sha256 pins, keeps those servers warm in one daemon per workspace, and answers a small set of verbs (`test`, `problems`, `find`, `debug test`, `show`, …) with compact, located, paged results. The same results reach the agent through the CLI, one MCP endpoint and an opt-in hook. Itse is an environment, not a decider: it starts nothing that isn't declared, runs no background work, never writes into the repos it serves, and its only on-by-default behaviour is a local usage log (`ITSE_USAGE=off` stops it). Out of scope: anything root-only (system libraries, docker; those are Halti images), version managers such as mise, refactoring, test selection, and supervision.

The design is [docs/design.md](docs/design.md). This tree is at slice S1: the workspace daemon, `status`, `show` over stored results, the usage log and `itse stats`, the MCP endpoint (`itse mcp`, a relay to the daemon) and the verb index (`itse verbs`, [docs/verbs.md](docs/verbs.md)); the resolver, actions, the LSP and DAP clients and the harness integration arrive with S2 to S6.

## Build and check

Rust 1.92.0 (pinned in `rust-toolchain.toml`), cargo and python3 (3.11+, for `tomllib`).

```sh
cargo build
cargo install --path . --bin itse     # itse-fixture is test-only and never installed
scripts/check.sh                      # fmt, clippy, tests, budget self-test, budget gates
scripts/check.sh --opt-in             # plus the #[ignore]d tests that need ITSE_E2E_* tools
```

The gates run locally, before every commit; there is no CI. Each prints `PASS <gate> <value>/<limit> (<seconds> s)` or `FAIL <gate>: <reason>`, and they fail closed. The budgets are in `budget.toml`: a static binary of at most 5 MiB, at most 60 crates (an exact allowlist per target), at most 5,000 code lines with a cap per module, at most 1,000 lines per first-party extension, and a daemon VmHWM of at most 20 MiB (from S1).

## State

Itse keeps everything in `~/.config/itse`, `~/.local/share/itse`, `~/.local/state/itse` and `~/.cache/itse` (or the `XDG_*_HOME` equivalents), never in a workspace.
