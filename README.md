# itse

A small workbench for coding agents and their harnesses, inside a Halti box. Itse reads a workspace manifest (suggesting entries from files it detects, never enabling them by itself), installs the declared language servers, debug adapters and tools into user space from sha256 pins, keeps those servers warm in one daemon per workspace, and answers a small set of verbs (`test`, `problems`, `find`, `debug test`, `show`, …) with compact, located, paged results. The same results reach the agent through the CLI, one MCP endpoint and an opt-in hook. Itse is an environment, not a decider: it starts nothing that isn't declared, runs no background work, never writes into the repos it serves, and its only on-by-default behaviour is a local usage log (`ITSE_USAGE=off` stops it). Out of scope: anything root-only (system libraries, docker; those are Halti images), version managers such as mise, refactoring, test selection, and supervision.

`itse verbs` lists the verbs and their arguments; `itse --help` gives the short form.

## Build and install

Rust 1.92.0 (pinned in `rust-toolchain.toml`) and cargo; the tests also run git, tar and gzip.

```sh
cargo build
cargo install --path . --bin itse     # itse-fixture is test-only and never installed
cargo test
```

`bench/` holds itse-bench, a separate crate that runs coding-agent harnesses on the same tasks with and without itse and compares the two; see `bench/README.md`.

## State

Itse keeps everything in `~/.config/itse`, `~/.local/share/itse`, `~/.local/state/itse` and `~/.cache/itse` (or the `XDG_*_HOME` equivalents), never in a workspace.
