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

## Claude Code

`itse integrate claude-code --skill --mcp --recipe post-edit` prints each file, JSON pointer and command it would change or run; `--yes` does it, and `--remove --yes` takes back exactly that. `--skill` writes `~/.claude/skills/itse/SKILL.md`, the same in every workspace; `--mcp` runs `claude mcp add-json itse … --scope local` in the workspace root, so the endpoint is registered for that root only, and itse never edits `~/.claude.json` itself, nor removes a server it didn't add; `--recipe post-edit` adds a `PostToolUse` hook to `~/.claude/settings.json`, keeping every other key but writing them sorted. `--remove` puts that file back byte for byte if it is unchanged since, else takes out only the hook. With `CLAUDE_CONFIG_DIR` set, all three go where it says; `--home DIR` puts `~` at DIR for claude too. Targets in a git work tree or a workspace are refused. The hook, `itse hook post-edit`, never starts the daemon or a language server: after an edit it asks the running ones for new errors and warnings in the edited file, and says nothing when there are none or they haven't settled within 1.5 s.
