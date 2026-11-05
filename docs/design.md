# Itse I1 design: the essential core

Status as of 2026-11-04. This is the one design for I1 (milestone "I1 · The essential core", due 2026-12-19). It merges four sub-designs: the dependency and extension ecosystem, the session host, verbs and exposure, and the repo and plan. Where they disagreed, this document picks one answer and says why (§0.3). It was revised the same day after two adversarial reviews; §15 lists what changed and where a reviewer was not followed. Nothing here is built yet. Numbers marked "measured" were measured in this box on 2026-11-04; everything else is a plan.

## 0. Status and decisions

### 0.1 Owner decisions this design follows

These are binding. The source is the owner's decision list of 2026-11-01, 11-02 and 11-04.

| # | Decision | Where it shows up here |
|---|---|---|
| — | Itse is an environment, not a decider. Nothing optional is forced. Standard protocols (LSP, DAP, BSP, MCP) are the contracts. Harness glue is extensions. Use, don't rebuild. | §1, §4, §7 |
| — | Four-part core: manifest and resolver, session host, verbs with agent-shaped results, exposure (CLI, one MCP endpoint, generated verb index). | §2 |
| — | Budgets: binary ≤ 5 MB, ≤ 5K non-test lines, ≤ 60 crates, daemon ≤ 20 MB RSS, first-party extensions ≤ 1K lines. I1 due 2026-12-19, I2 2027-01-30. | §11, §12 |
| 1 | Rust, in Halti's style: sync std threads, mpsc, no tokio, serde_json, anyhow, rustfmt `max_width = 180`, `//!` module docs, fakes behind traits, re-exec and e2e tests on the built binary, lowercase errors. No clap. One crate; small separate crates only where needed (`bench/`). | §11 |
| 2 | The core opens no network sockets except its own Unix socket. Bytes move only through delegated tools (curl in I1) through Halti's door. The core verifies sha256 pins. | §4.5, §10 |
| 3 | Gates run locally only: a check script (fmt, clippy `-D warnings`, tests, budget gates that fail closed). No GitHub CI. Rust pinned to 1.92.0. | §12 |
| 4 | Itse manages each language's dependencies itself, in user space, without root. Its own small data-only ecosystem: an extension is one JSON file. It reuses VS Code tasks.json/problemMatcher verbatim, Open VSX `server-bundle.json` and GitHub release digests as pin sources. | §4 |
| 5 | Itse fetches by itself, never through mise, not even optionally. Itse owns its pin-source adapters. | §4.5 |
| 6 | Human-only actions (trust, consent, ext add, TOFU pins) are advisory in I1. MCP returns `needs_human`. Touch ID enforcement is I2. | §6.1, §10 |
| 7 | Halti changes are filed only (EMA-214, EMA-215, EMA-216). Itse's tests must not depend on them. | §4.6, §12 |
| 8 | Itse never writes into client repos. All state is in `~/.config/itse`, `~/.local/share/itse`, `~/.local/state/itse`, `~/.cache/itse`. | §3, §4, §10 |
| 9 | Usage log (EMA-212): local `usage.jsonl`, on by default, `ITSE_USAGE=off`, never logs values, no upload ever, `itse stats` CLI only. | §8 |
| 10 | Baseline (EMA-213): itse-bench, a separate crate under `bench/`, Harbor-shaped tasks, two arms, declared harness adapters. | §9 |
| 11 | Proof: build and test the whole core in this box against fixture servers and real rust-analyzer and cargo. The Kotlin acceptance happens later in a JVM box. | §12, §13 |

Later owner decisions that change the ecosystem design: no mise ever, and no repo writes. So `itse init --team` is dropped, and the lock lives in machine state. A team layer in a repo (`.itse/`) is still read if it is present, never written. The developer layer is written only by `itse use`/`itse unuse`, under `~/.config/itse` (§4.3).

**Reading of decision 8 used here**: Itse itself writes no file under a workspace root, including `.git/`. The project's own declared tools (`cargo test`, `gradlew`, a trusted rust-analyzer's `cargo check`) still write what they always write (`target/`, `build/`, `.gradle/`, `Cargo.lock`). §10 states the rule and §12.4 tests it.

### 0.2 Probes behind this design (2026-11-04, this box)

- **rust-analyzer 1.92.0** accepts `positionEncodings: ["utf-8"]`. It advertises pull diagnostics, but `textDocument/diagnostic` returned `items: []`; the `cargo check` errors came only through `publishDiagnostics`. `didSave` re-runs flycheck about 0.2 s later, even for unopened files, as long as the client does not advertise watched-file registration.
- **rust-analyzer 1.92.0 config names** (from `--print-config-schema`): `rust-analyzer.checkOnSave`, `rust-analyzer.cargo.buildScripts.enable`, `rust-analyzer.procMacro.enable`, `rust-analyzer.cargo.targetDir`, `rust-analyzer.check.command`. In `workspace/symbol` replies, `containerName` is only the impl's self type (`Region`, not `data::Region`), free functions have `containerName: null`, and methods come back as kind 12 (Function), not 6 (Method) (reviewer probe).
- **cgroups** are not writable by uid 1000. Raising `/proc/<pid>/oom_score_adj` works. `XDG_RUNTIME_DIR` and `XDG_STATE_HOME` are unset. `/tmp` is a 4 GB tmpfs.
- **Claude Code 2.1.312 env and CLI**: Bash commands see `CLAUDE_CODE_SESSION_ID` (a hyphenated UUID) and `AI_AGENT` (`claude-code_<version>_agent`). Hook stdin carries `session_id` and `cwd`. `claude mcp add-json <name> <json> --scope local|user|project` and `claude mcp remove <name> --scope …` exist. MCP tool schemas are deferred behind tool search: the agent first sees only tool names and each server's `instructions`.
- **Replaced binaries**: after `cargo build` replaces the file, a running process's `current_exe()` reads `…/itse (deleted)` and spawning it fails with ENOENT; spawning `/proc/self/exe` still works (reviewer probe).
- **No debug adapter** is installed in this box (no gdb, lldb-dap, codelldb or debugpy). DAP is proven here against the fixture only.
- **Claude Code 2.1.312's bundled MCP client** knows `2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05, 2024-10-07` for `initialize`, plus a "modern" `2026-07-28` era with `server/discover`. It shows a result's text block unchanged and builds text from `structuredContent` only when no text block exists. Its PostToolUse hook output is `hookSpecificOutput{hookEventName, additionalContext, classifierContext}`.
- **libtest JSON** needs nightly. `RUSTC_BOOTSTRAP=1` in cargo's environment rebuilds anyhow. This design does not use libtest JSON (§6.4).
- **Crates**: anyhow, serde, serde_json, sha2, lexopt and regex-lite resolve to 25 crates on aarch64, 24 on x86_64. A static glibc build of that set is 1.06 MB.
- **Toolchain**: `rust-toolchain.toml` with `channel = "1.92.0"` resolves offline to the installed toolchain. No musl target is installed.

### 0.3 Contradictions resolved in this document

| # | Topic | Sub-designs said | Chosen | Why |
|---|---|---|---|---|
| C1 | Daemon socket protocol | host: JSON-RPC 2.0 with `hello` first; verbs: a service line (`cli\n`, `mcp\n`, `hook\n`) then a custom request | JSON-RPC `hello {proto, mode: cli\|mcp\|hook, env, …}` | One framing, a protocol version for upgrades, and the caller's env travels with the call. `mode` does the job of the service line. |
| C2 | Handle names | host: `lsp:x`, `mcp:x`, `dap:N`, `run:N`; verbs: `run:N`, `q:N`, `dbg:N` | `lsp:<id>`, `mcp:<id>`, `run:N`, `dbg:N`, `q:N` | Server handles name the protocol; agent-facing sessions and results use the verbs' names, which the examples and skill already use. |
| C3 | State and lock paths | host: `~/.local/state/itse/ws/<id>/`, lock in the runtime dir; plan: `<state>/<id>/`, `daemon.pid` | host's layout, `daemon.json` (holds the pid); `daemon.lock` and `itse.sock` in `ws/<id>/` too, `/tmp` only when the socket path would overflow | One per-workspace dir under `ws/`, so global files (`usage.jsonl`, `consents.json`) don't mix with workspace dirs. The lock path must not depend on the caller's `XDG_RUNTIME_DIR`, or a shell and a harness would start two daemons for one workspace (review). Decision 8 lists the four state dirs. |
| C4 | Idle times | host: servers 30 min, daemon 5 min after everything is idle; plan: daemon 1,800 s, LSP 600 s | host's | A kotlin-lsp cold import takes minutes, so servers stay 30 min. The daemon only lingers 5 min once nothing is running. |
| C5 | Hosted MCP aggregation | ecosystem: I2; verbs: in I1, but first to cut; plan: I1 (S6); host: a minimal client in I1 | Minimal in I1 (client, `expose: true` listing, pass-through calls); the rest I2 | The milestone requires "one hosted MCP server". The minimal part is about 90 lines. |
| C6 | Tool name namespacing | host and plan: `<server>__<tool>`; verbs: `<ext>_<tool>` | `<server>__<tool>` | The double underscore splits unambiguously; ids have `-` mapped to `_`. |
| C7 | MCP versions served | Halti: echo; verbs: legacy list plus a modern era; plan: one list after a probe | Serve `2025-11-25` and `2025-06-18` in S1. The `2026-07-28` era (`server/discover`) comes after the week-1 probe, from the line reserve | The modern details come from Claude Code's bundled schemas, not the spec. Building on unverified wire details risks a rewrite. |
| C8 | Tool list | ecosystem: `status`, `install`, `job`, `run_action`; verbs: `query`, `run`, `show` (+`debug`) | verbs' three-tool shape, renamed `test_build_run`, `problems_symbols`, `show_result`; `debug` only on opt-in | Measured at about 430 tokens (640 with `debug`). Jobs are runs, so `job` becomes `show run:N`. Claude Code defers schemas behind tool search, so names carry the keywords (§7.4). |
| C9 | cargo results | verbs and ecosystem: an in-core `cargo-json` parser plus a libtest JSON runner trick; plan: problemMatchers in `ext/rust` | problemMatchers in `ext/rust`, no cargo-json, no `RUSTC_BOOTSTRAP` | Owner rule: nothing language-specific in the core. Saves about 70 lines and avoids an unstable libtest flag. |
| C10 | Regex crate | host and ecosystem: `regex`; plan: `regex-lite` | `regex-lite` | +1 crate and 66 KB, against +3 crates and up to 0.8 MB. Same syntax limits (no lookaround or backreferences), which the ecosystem already rejects. |
| C11 | Parent-death signal | plan: `prctl(PR_SET_PDEATHSIG)` for children | Not used | Linux sends it when the *spawning thread* exits, and servers are spawned from connection threads. Leftovers are killed from `procs.json` instead (§5.3). |
| C12 | Fixture servers | host: `src/fixture_tests.rs`; plan: `src/fixture.rs` plus `src/fixture/` | a second binary target `itse-fixture` at `tests/fixture/main.rs` | Test code lives under `tests/`, so it is outside the 5K by the ordinary reading, needs no `test_paths` exception and no fixture-guard gate, and is never in the shipped binary (review). |
| C13 | LSP sync before a query | verbs: `didOpen`/`didChange` files in scope, wait 300 ms quiet, 3 s max; host: stat open documents, `git status` and `didSave` for the rest, settle on progress and quiescence | host's | Verified against rust-analyzer: `didSave` re-checks unopened files. Opening every file in scope costs more and is not needed. |
| C14 | Pull diagnostics | verbs: pull first when offered; host: push first, pull merged | Push only in I1 | rust-analyzer's pull returned nothing. Pull merging is the first item on the cut list, so it is cut now. |
| C15 | `problems` wait | verbs: 3 s max; host: settle up to 20 s | host's settle rules; deadline 20 s for `problems`, 1.5 s for `hook` | The result says `settled: false` when the deadline hits, so short waits are honest, not wrong. |
| C16 | Debug snapshot caps | host: 20 levels, top 3 frames, 25 vars, 160 chars, ±3 lines; verbs: 40 levels, top frame, 20 vars, 100 chars, ±2 lines | 40 levels; locals collected for the top 3 project frames, frame 1 rendered; 20 vars per scope, 10 children; 160 chars; ±2 lines; text ≤ 4,000 chars | JVM stacks under Spring are deep, so 40 levels. 160 chars matches the source-line clip. Rendering one frame keeps the page under the cap; `snapshot --frame F2` shows the others. |
| C17 | Cancel and timeout | host: SIGINT, then group SIGTERM after 5 s, then SIGKILL after 5 s; plan: timeout sends SIGTERM, SIGKILL after 5 s | host's escalation, for both cancel and timeout | One rule. SIGINT first gives Gradle and cargo a clean stop. |
| C18 | Usage `ws` value | host: workspace basename; verbs: 12-hex hash | 12-hex hash of a per-machine salt plus the root | A basename is part of a path, and EMA-212 never logs paths. An unsalted hash of `/work/crm` can be reversed by guessing (review). |
| C19 | `ITSE_RUN` | host: sent in `hello`; analytics proposal: cut | Cut | Each bench trial has its own `XDG_STATE_HOME`, so no join key is needed. |
| C20 | Exit code when the daemon won't start | host: 1 | 2 | 1 means "the verb worked and reports failure" (§6.1). |
| C21 | Harness writes and the index | verbs: `itse integrate <ext>`, `itse verbs`; plan: `itse ext apply claude-code --yes`, `itse index --md\|--skill` | `itse integrate <ext> [--yes]` (human-only), `itse verbs [--md\|--json]` | `integrate` names what it does for any harness extension. Without `--yes` it prints the changes; with it, it writes or runs the harness's own CLI. It is human-only because it installs hooks and MCP servers into the harness (review). |
| C22 | `doctor` | ecosystem and verbs: `doctor [--hosts]`; plan: fold into status if lines run short | `status [--hosts]`, no `doctor` | EMA-195 already reads "doctor" as `status`. One verb less. |
| C23 | Result envelope | verbs: `Outcome` with `{"itse":1, …}`; plan: `{verdict, items, text, full_bytes, id}` | verbs' `Outcome` | It already carries the ecosystem's per-item state rows (§6.2). |
| C24 | Result retention | verbs: 200 runs or 512 MB; plan: 200 or 500 MB | 200 results or 512 MiB per workspace | A power of two, nothing more. |
| C25 | Where consents and found tools live | plan: per workspace; ecosystem: global | Consents and `found.json` global; lock and trust per workspace | Consent is to a package's EULA hash, not to a repo. Trust is about a repo's code. |
| C26 | Bench hashing | plan: shell out to `sha256sum`; EMA-213: only crates the core already uses | `sha2` in bench | sha2 is a core crate, so it is allowed, and a missing tool can't break the bench. |
| C27 | Line budget | host: ≈4,940 total; verbs: ≈5,460 (over); plan: caps summing to 4,770 | One cap table summing to 4,800, with 200 in reserve (§11.3) | Fits the 5K cap. The cuts are listed. |
| C28 | Usage off switch per call | verbs: a `caller.usage` field | Read `ITSE_USAGE` from `hello.env` | The caller's env already travels in `hello`; no extra field. |

## 1. What Itse is

Itse is a small workbench that coding agents and their harnesses use inside a Halti box. It reads a workspace manifest (suggesting entries from files it detects, never enabling them by itself), installs the declared language servers, debug adapters and tools into user space from sha256 pins, keeps those servers warm in one daemon per workspace, and answers a small set of verbs (`test`, `problems`, `find`, `debug test`, `show`, …) with compact, located, paged results. The same results reach the agent through the CLI, one MCP endpoint and an opt-in hook. Itse decides nothing for the agent: it starts nothing that isn't declared, runs no background work, and its only on-by-default behaviour is a local usage log the developer can turn off. The opt-in hook never starts anything; it reads what already runs.

## 2. Architecture overview

```
 harness (Claude Code, Henki, a shell)
   │  Bash: itse test :data          │ MCP stdio: itse mcp          │ hook: itse hook post-edit
   ▼                                  ▼                               ▼ (connect only, never starts)
 itse CLI ───────────────── connect (auto-start) ─────────────── Unix socket 0600
                                                     ~/.local/state/itse/ws/<id>/itse.sock
 ┌──────────────────────── itse daemon (one per workspace) ────────────────────────────┐
 │ exposure     serve_conn: hello → cli/hook calls │ mcp::serve_io (MCP server)        │
 │              ───────────── dispatch::call (args, trust, run, shape, store, usage) ── │
 │ verbs        status use ext install lock trust consent run build test problems     │
 │              find info refs outline debug show stop cancel gc                       │
 │ results      Outcome → text/JSON, caps and pages, results store (show <id>)         │
 │ session host registry of handles: lsp:<id> mcp:<id> dbg:N run:N                     │
 │              rpc::Peer (Lines | Headers framing; Lsp | Dap | Mcp dialect)          │
 │ resolver     manifest layers → states → Launch; store; pins; env and shims          │
 │ usage        usage.jsonl writer (fail open)                                         │
 └──────┬──────────────┬───────────────┬──────────────┬───────────────────────────────┘
        │ stdio         │ stdio / tcp   │ stdio        │ setsid re-exec
   LSP servers     DAP adapters    MCP servers    /proc/self/exe __run <rundir> → action
   (rust-analyzer,  (loopback       (expose: true)  (gradlew, cargo); out.log, run.json
    kotlin-lsp)      connect, O1)                   survive the daemon
 delegates for bytes: curl (through the door), tar, gzip, python3 zipfile, keytool
```

**The four parts, and where they live:**

| Core part | Modules |
|---|---|
| Manifest and resolver | `manifest` (layers, detection, `use`), `resolve`, `lock`, `store`, `pins`, `env` |
| Session host | `workspace`, `daemon`, `rpc`, `host`, `lsp`, `dap`, `actions`, `mcp/client` |
| Verbs and results | `verbs`, `results`, `parse` |
| Exposure | `main` (CLI), `mcp` (server), `verbs` index, `integrate` and `hook` (in `verbs`), `usage` |

`sys` holds the hand-written FFI and small OS helpers that every part uses.

**Rules that cut across all parts:**
- Synchronous Rust: std threads, `mpsc`, `Mutex`/`Condvar`, `serde_json::Value` with a few derive structs, `anyhow`. No async runtime.
- Children inherit the box environment. Never `env_clear`. Commands are argv vectors, never shell strings (except a declared `type: shell` action, §5.6).
- Itse writes nothing under a workspace root (§0.1 reading of decision 8).
- Every result is located (path:line:col with the source line), capped and paged, with the full output reachable by id.

## 3. Workspace, daemon and state

### 3.1 Workspace and repo identity (`workspace`)

- **Root**: `--root <dir>`, then `ITSE_WORKSPACE`, then the nearest ancestor of the cwd with a `.git` directory or file. A worktree or submodule has a `.git` file, so each is its own workspace (its own daemon, servers and results). With no `.git`, the cwd is used and `status` says `workspace: <dir> (not a git work tree)`. The root is canonicalized.
- **Repo key**: the canonical git common dir, read from files without running git. A `.git` dir is its own common dir. A `.git` file holds `gitdir: <dir>`; if `<dir>/commondir` exists (a worktree), the common dir is `<dir>/<commondir>`, else `<dir>` itself (a submodule). With no git, the repo key is the root. The repo's display name is the common dir's parent for a standard `.git`, else the root. A test checks this against `git rev-parse --git-common-dir` when git is on PATH.
- **What is keyed by what**:

  | Keyed by the repo (all worktrees of one clone share it) | Keyed by the worktree root |
  |---|---|
  | developer layer (`itse use`), trust, the machine lock | daemon, socket, results, runs, LSP and DAP state, `--new` baselines |

  So a fresh worktree made by a subagent or `EnterWorktree` starts with the repo's declared entries, pins and trust, and `status` says `layer: from repo /work/crm`. A team layer is still read from the worktree's own checkout, so a branch with a different `.itse/` asks for trust again (§10).
- **Ids**: `ws` = the first 12 hex digits of sha256(canonical root), `rid` = the same of the repo key. The name shown to people is the basename. The usage log uses a salted variant (§8.2).
- **Dirs**: each of the four dirs is `$XDG_*_HOME/itse` when that is absolute, else under `$HOME`; if `HOME` is unset, empty or relative and the XDG var is missing, every verb fails with `HOME is not an absolute path and XDG_STATE_HOME is unset; itse keeps its state under them` (exit 2), since a relative dir would land in the client repo, the daemon's cwd.
- **Socket and lock**: `~/.local/state/itse/ws/<ws>/itse.sock` and `daemon.lock`, in a 0700 dir. Typical: `/home/agent/.local/state/itse/ws/3f9a0c21b7e4/itse.sock`, 55 bytes. The path never depends on `XDG_RUNTIME_DIR`. Only if the socket path would exceed 107 bytes (a long test `HOME`) does the socket move to `/tmp/itse-<uid>/<ws>.sock` (always `/tmp`, never `TMPDIR`, which differs between a shell and a harness; a fallback path still over 107 bytes is an error); the lock always stays in `ws/<ws>/`. The `/tmp` dir is `lstat`ed and must be a real dir owned by us with `mode & 0o077 == 0`, otherwise: `socket dir /tmp/itse-1000 is not private (owner 0, mode 777); remove it`. The socket is chmod 0600 after bind.

### 3.2 Disk layout

```
~/.config/itse/config.json                 developer config: usage, daemon_idle_minutes, server_idle_minutes, mcp.debug
~/.config/itse/workspaces/<name>.json      developer layer for one repo; holds "repo": <repo key>; written by itse use/unuse
~/.config/itse/extensions/<id>.json        added extensions (itse ext add)
~/.local/share/itse/store/sha256-<hex>/    read-only packages
~/.local/share/itse/dl/                    resumable downloads (<sha>.part)
~/.local/share/itse/bin/                   shims (symlinks to itse)
~/.local/state/itse/salt                   32 random bytes, used only for the usage log's ids
~/.local/state/itse/usage.jsonl            usage log (+ usage.1.jsonl)
~/.local/state/itse/consents.json          consents, per package and EULA hash
~/.local/state/itse/found.json             `path` resolutions: path, version, binary sha256
~/.local/state/itse/jvm/door-<sha>.p12     JVM truststore built from the door CA
~/.local/state/itse/repo/<rid>/itse.lock.json   pins for this repo (never in the repo)
~/.local/state/itse/repo/<rid>/trust.json  {repo, accepted: [trust hash ×≤8], actor, at}
~/.local/state/itse/ws/<ws>/root           the workspace path (for gc of orphans)
~/.local/state/itse/ws/<ws>/itse.sock      daemon socket (0600)
~/.local/state/itse/ws/<ws>/daemon.lock    flock held by the live daemon
~/.local/state/itse/ws/<ws>/daemon.json    {pid, start_time, proto, version, exe_dev, exe_ino, started_at, started_by, env_keys_sha, root}
~/.local/state/itse/ws/<ws>/daemon.log     daemon stderr (rotated by the starting CLI past 4 MB)
~/.local/state/itse/ws/<ws>/procs.json     live processes: handle, kind, pid, pgid, start_time, argv0, log
~/.local/state/itse/ws/<ws>/next.json      handle counters (run, dbg, q), never reused
~/.local/state/itse/ws/<ws>/baselines.json `--new` baselines by session key: fingerprints, at
~/.local/state/itse/ws/<ws>/servers/<id>.log   server stderr (renamed to .1 past 8 MB at spawn)
~/.local/state/itse/ws/<ws>/runs/<n>/      run.json (spec, pid, exit) out.log result.json
~/.local/state/itse/ws/<ws>/results/<n>.json   stored results of queries (q:N)
~/.local/state/itse/ws/<ws>/debug/<n>/     snapshots of dbg:N (s1.json, s2.json, …)
~/.cache/itse/                             server caches (kotlin-lsp system path, rust-analyzer target dir)
<repo>/.itse/itse.json, itse.lock.json     team layer: read if present, never written
```

All of `~/.local/state`, `~/.local/share` and `~/.config` persist in a Halti box (`/home/agent`). Developer-layer files are named after the repo (`crm.json`, `crm-2.json` on a name clash) and found by their `repo` field, so a human can find and edit them; moving a clone means editing that one field.

### 3.3 Daemon lifecycle (`daemon`)

**Auto-start.** Every command connects to the socket, except `--version`, `help`, `stats`, `verbs`, `env`, `exec`, `integrate`, `use`, `unuse`, `daemon stop`, the internal `__run` and `__daemonize`, argv[0] shims, and **`hook`, which never starts the daemon** (it prints nothing and exits 0 when no daemon runs). On `ENOENT` or `ECONNREFUSED` the CLI rotates `daemon.log` if it is over 4 MB, then spawns `/proc/self/exe __daemonize --root <root>`. That intermediate process spawns the daemon with `setsid()` in `pre_exec` (so a harness killing the Bash tool's process tree doesn't kill it), stdin null, stdout and stderr appended to `daemon.log`, cwd = root, and exits at once; the CLI reaps it. So the daemon is never the child of a long-lived `itse mcp` relay and never becomes its zombie. The CLI retries `connect` every 20 ms for 10 s, longer than the daemon's 3 s shutdown cap. On failure: `itse: the daemon didn't start (see …/daemon.log)` plus the log's last 3 lines, exit 2. The `hello` reply has a 5 s timeout: `itse: the daemon at … did not answer hello in 5 s (pid 4242); 'itse daemon stop' kills it`.

**Daemon env.** The daemon keeps its starter's env minus `ITSE_*`, `CLAUDE_*` and `AI_AGENT`, as the base for servers (§4.6). `daemon.json` records `started_by` (`cli` or `mcp`) and a hash of the env keys; `status` shows both, so a server that behaves differently after a restart can be traced to a different starter.

**One instance.** The daemon opens `daemon.lock` and polls `File::try_lock()` (std, Rust 1.89+) every 100 ms for up to 10 s. That covers a previous daemon that is still shutting down. Still `WouldBlock` after 10 s while the socket answers: another daemon is alive, exit 0 quietly; two racing CLIs end up with one daemon. The holder removes a stale `itse.sock`, binds, chmods, and writes `daemon.json`. The flock goes away with the process, so no stale-lock cleanup is needed.

**Threads.** The accept loop; one thread per connection (Halti door.rs pattern) with `catch_unwind` per request, so a panic can't take down warm servers; one reader per protocol peer (default stack size); one waiter per run; one reaper ticking every second for flags and every 10 s for idle and liveness. All mutexes are taken through one helper, `lock(&m)` = `m.lock().unwrap_or_else(PoisonError::into_inner)`, so a panic that poisons a lock can't disable a server for the daemon's lifetime. `sys` calls `mallopt(M_ARENA_MAX, 2)` at daemon start, so each allocating thread doesn't get its own glibc arena.

**Waking the accept loop.** Halti's `catch_signals` installs handlers with glibc `signal()`, which sets `SA_RESTART`, and std retries `EINTR`, so a blocked `accept()` never sees a flag. The reaper therefore checks the signal flag and the idle deadline every second, and when either fires it connects to the daemon's own socket, which wakes `accept`; the accept loop checks the flags after every accept. The reaper also `stat`s the socket path each tick and re-binds if the file is gone or has another inode.

**Idle.**
- Servers stop after `server_idle_minutes` (default 30; per server `idle_minutes`, 0 = never).
- Debug sessions stop after 10 minutes.
- The daemon exits after `daemon_idle_minutes` (default 5) with no connections, live servers, running runs or debug sessions. A connected MCP client keeps it alive; servers still idle-stop.
- `ITSE_DAEMON_IDLE_S` and `ITSE_SERVER_IDLE_S` override for tests. They are real config, in release builds too.

**Signals and exit.** SIGTERM, SIGINT and SIGHUP set a flag (Halti run.rs `catch_signals`; `signal` is declared in `sys`). A graceful exit, in order: close the listener and unlink the socket (so new CLIs start a fresh daemon that waits on the lock); send LSP `shutdown`/`exit`, close MCP stdin and send DAP `disconnect`, all in parallel with one 3 s cap for everything, then `kill_group` what is left; leave runs running (they are detached, §5.6); exit, which drops the flock. The daemon also exits if the root disappears.

**Version mismatch.** `hello` and `shutdown` never change across protocol versions. `hello` carries the CLI's exe `(dev, inode)`.
- Different `proto`: the daemon answers -32003 with `{daemon_proto, daemon_version}`. The CLI sends `shutdown {reason: "proto"}` and starts its own daemon, which waits on the lock as above.
- Same `proto`, a different exe inode (the binary was rebuilt or reinstalled): if no runs or debug sessions are live, the CLI sends `shutdown {reason: "upgrade"}` and starts the new daemon; warm servers restart cold and the result says so. Otherwise the old daemon serves the call and `status` says `daemon 0.1.0 (replaced binary) stays until run:14 ends`. The old daemon spawns everything through `/proc/self/exe`, never `current_exe()`, so it keeps working after its file is replaced (§0.2).

### 3.4 CLI↔daemon protocol (ITSE-RPC proto 1)

Newline-delimited JSON-RPC 2.0 over the Unix socket. The server half is `daemon::serve_conn(reader: impl BufRead + Send + 'static, writer: impl Write + Send + 'static, host: &Arc<Host>)`, Halti's `serve_io` shape (mcp.rs:127), so tests drive it over `UnixStream::pair()`.

```
→ {"jsonrpc":"2.0","id":0,"method":"hello","params":{"proto":1,"version":"0.1.0","exe":[64769,1835012],"exe_path":"/home/agent/.cargo/bin/itse","mode":"cli","cwd":"/work/app/core",
     "env":{"PATH":"…","JAVA_TOOL_OPTIONS":"…","CLAUDE_CODE_SESSION_ID":"…"}}}
← {"jsonrpc":"2.0","id":0,"result":{"proto":1,"version":"0.1.0","pid":4242,"root":"/work/app","ws":"3f9a0c21b7e4","started":"2026-11-04T18:00:00Z"}}
→ {"jsonrpc":"2.0","id":1,"method":"call","params":{"verb":"test","args":{"target":":data","rest":["OrderRepositoryTest"],"flags":{}},
     "json":false,"page":1,"wait":100}}
← {"jsonrpc":"2.0","id":1,"result":{"exit":1,"text":"FAIL test :data (gradle:test) — 2 failed, 141 passed; 38.2 s; run:14\n…","outcome":{"itse":1,…}}}
```

- `hello` comes first; anything else gets -32002 `hello required`.
- **Caller identity** is derived by the daemon from `hello.env`, never sent as values of its own:
  - `session`: `ITSE_SESSION` if set; else the `identity.session_env` declared by a harness extension (`ext/claude-code`: `CLAUDE_CODE_SESSION_ID`), the first one set wins; for `hook` mode, the stdin field named by `identity.hook_session_from` (`/session_id`). The value is hashed to 16 hex digits (sha256 of salt + value) before any use, so any format is accepted and none is stored. With none of these, the CLI's session key is `cli` (one per workspace).
  - `client`, `client_v`: `ITSE_CLIENT` (`name/version`), else the extension's `identity.client_env` parsed by its `client_regexp` (`AI_AGENT=claude-code_2-1-312_agent` → `claude-code`, `2.1.312`), else `cli`.
  - The core reads only env names that extension data declares; it has no harness names of its own.
- `env` is the caller's whole environment. It is the base env for actions started by this call, and the source of `ITSE_USAGE` for this call. It is never written to disk or logged.
- **Modes.** `cli` and `hook`: then `call`, `ping`, `status`, `shutdown {reason}`. `mcp`: after the hello reply the connection becomes an MCP session served by `mcp::serve_io` (§7.2).
- **Cancel.** One call per CLI connection. Client EOF during a call cancels a *query* down to its server (§5.1). A *run* is a handle and keeps going (detach). The CLI reads the reply with `set_read_timeout(100 ms)` in a loop that checks its signal flag; on the first Ctrl-C it sends `$/cancelRequest {id}`, which cancels the call, and for a run started by that call, cancels the run (§5.6). A second Ctrl-C exits at once (exit 130). `itse cancel run:14` cancels later.
- **Errors.** Only protocol faults are JSON-RPC errors: -32700, -32600, -32601, -32602, -32002, -32003. A verb that fails returns a result with its verdict (Halti's `isError` rule).
- The daemon renders the text, so the CLI and MCP print identical bytes.

## 4. Extensions, packages and the resolver

### 4.1 Vocabulary

- **Extension**: one JSON file (`itse-ext.json`), data only, at most 1K lines. It declares packages, servers, actions, problem matchers, consents, checks, detection files, skills and harness integration and identity data. Origins:
  - *built-in*: in the binary via `include_str!` from `ext/<id>/itse-ext.json`. Launch data only, never pins. I1 built-ins: `rust`, `jdk`, `kotlin`, `gradle`, `node`, `bun`, `claude-code`. A built-in is available, not used: its servers and actions count only once a layer names it in `use`.
  - *added*: `itse ext add <file>`, copied to `~/.config/itse/extensions/` and recorded by sha256 (human-only).
  - *team*: `.itse/itse.json` in the repo. Read-only, used only after `itse trust`.
- **Package**: a purl, a `bin` map, `hosts[]`, a `pin_source`, and optional `expires_after_days`, `consents`, `exit_codes`, `jvm_args`, `jvm_args_env`, `executes_project_code`.
- **Pin**: `{url, sha256, size, provenance}` per target, only in a lock. Provenance: `upstream:github-digest`, `upstream:openvsx-bundle`, `upstream:sha256-file`, `image`, or `tofu` (policy, open point O8).
- **Lock**: `~/.local/state/itse/repo/<rid>/itse.lock.json`, fetched packages only, keyed by target, shared by the repo's worktrees. Never in the repo.
- **Store**: content-addressed, read-only, `~/.local/share/itse/store/`.
- **Layers** (EMA-195's three, plus where extensions come from):
  - *detected*: suggestions only, never used until a human or agent runs `itse use`. Each extension's `detect` lists files whose presence suggests it (`rust`: `Cargo.toml`; `gradle`: `gradlew`, `settings.gradle.kts`, `settings.gradle`; `kotlin`: `settings.gradle.kts`, `build.gradle.kts`; `jdk`: through `requires`; `node`: `package.json`; `bun`: `.bun-version`, `bun.lock`). Detection reads only the root's top-level names; nothing language-specific is in the core.
  - *team*: `.itse/itse.json` in the repo. Read-only; its entries run only after `itse trust`.
  - *developer*: `~/.config/itse/workspaces/<name>.json`, per repo, written only by `itse use`/`itse unuse` or by hand.
  - Precedence: developer over team; built-in and added extensions supply the definitions that `use` names. The higher layer replaces an entry by name; no deep merge.
- **Tiers**: T0 image (root-only, Halti's), T1 project (the repo's own managers such as gradlew and `npm ci`: checked, never installed), T2 Itse (user-space toolchains and tools).

### 4.2 What is reused, in which form

| Source | Form | I1 | Later |
|---|---|---|---|
| VS Code tasks.json and problemMatcher | `actions[]` format verbatim: `label`, `type: process\|shell`, `command`, `args`, `options`, `dependsOn`, `group`, `problemMatcher` | single-line and fixed multi-line patterns, `absolute`/`relative` | `loop`, background matchers, reading `.vscode/tasks.json` |
| Open VSX `server-bundle.json` (never the VS Code marketplace) | pin source | yes | debugger schemas, VSIX as a container |
| GitHub release asset `digest` | pin source | yes | `gh attestation verify` |
| nvim-lspconfig / Helix launch stanzas | copied by hand into `servers[]` | yes | importer, outside the core |
| mason-registry | authoring import only, never at run time (no sha256 in its specs) | no | converter, outside the core |
| MCP registry `server.json` | embedded plus an Itse pin | no | yes |
| Repo pins | read, never rewritten: `gradle-wrapper.properties` (`distributionSha256Sum` present?), `gradle-daemon-jvm.properties`, `package.json` engines/packageManager, `.nvmrc`, `.node-version`, `.java-version`, `.bun-version` | yes | `.tool-versions`, `rust-toolchain.toml` |

Only payloads are launched (servers, adapters, CLIs). Other ecosystems' JS, Lua or WASM is never run.

### 4.3 Formats

**Built-in `kotlin` extension** (abridged):
```json
{ "itse_ext": 1, "id": "kotlin", "version": "0.1.0", "requires": ["jdk"], "detect": ["settings.gradle.kts", "build.gradle.kts"],
  "packages": { "kotlin-lsp": {
      "purl": "pkg:generic/jetbrains/kotlin-lsp", "install": "fetch",
      "pin_source": { "kind": "openvsx-bundle", "extension": "JetBrains/kotlin-server",
                      "platforms": { "linux_arm64": "linux-arm64", "linux_x64": "linux-x64" } },
      "archive": "tar.gz", "strip": 1, "bin": { "kotlin-lsp": "bin/intellij-server" },
      "hosts": ["open-vsx.org", "download.jetbrains.com"],
      "expires_after_days": 30, "exit_codes": { "7": "server_expired", "11": "needs_consent:eula" },
      "consents": [
        { "id": "eula", "kind": "hash", "of_file": "EULA.txt", "hex_prefix": 16, "flag": "--eula=" },
        { "id": "region", "kind": "choice", "env": "INTELLIJ_REGION" },
        { "id": "data_sharing", "kind": "choice", "env": "INTELLIJ_DATA_SHARING", "values": ["none","anonymous","full"] } ],
      "jvm_args": ["-Xmx2g"], "jvm_args_env": "IJ_JAVA_OPTIONS", "executes_project_code": true } },
  "servers": [
    { "id": "kotlin-lsp", "protocol": "lsp", "transport": "stdio",
      "command": ["${pkg:kotlin-lsp}/bin/intellij-server", "--stdio", "--system-path=${cache}/kotlin-lsp", "${consent:eula}"],
      "filetypes": ["kt","kts"], "languages": { "kt": "kotlin", "kts": "kotlin" },
      "env": { "JAVA_HOME": "${pkg:jdk}" }, "init_options": { "defaultSdk": "${pkg:jdk}", "buildTools": ["gradle"] } },
    { "id": "kotlin-dap", "protocol": "dap", "adapter_id": "intellij_debugger",
      "via": { "lsp_command": { "server": "kotlin-lsp", "command": "start_debug_server" } },
      "hide_vars": ["^\\$i\\$a\\$"] } ],
  "problemMatchers": { "kotlinc": { "owner": "kotlinc", "fileLocation": "absolute",
      "pattern": { "regexp": "^([ew]): file://([^:]+):(\\d+):(\\d+) (.*)$", "severity": 1, "file": 2, "line": 3, "column": 4, "message": 5 } } },
  "skills": [ { "name": "kotlin-module-scoped", "text": "Gradle runs per module: itse test :data." } ] }
```
The kotlin-lsp consent flags, exit codes and `start_debug_server` come from third-party sources and are unverified first-hand. The early JVM probe checks them (§13, P-JVM). kotlin-lsp is a rolling pin: builds expire about every 30 to 40 days, so the I1 proof uses whatever build is current then.

**Built-in `gradle` extension action** (Itse's additions to the VS Code format are `target_args`, `filter_args`, `parse`, `frames`, `classify`, `ready`, `drop_lines`, `outputs`, `executes_project_code`):
```json
{ "label": "gradle:test", "type": "process", "command": "${workspaceFolder}/gradlew", "group": {"kind":"test","isDefault":true},
  "args": ["${input:target}:test", "--console=plain", "--continue", "${filter_args}"], "filter_args": ["--tests", "${input:filter}"],
  "parse": ["junit-xml:**/build/test-results/**/TEST-*.xml"], "problemMatcher": ["$kotlinc"],
  "frames": { "regexp": "^\\s+at ([\\w.$]+)\\.([^.(]+)\\(([^:()]+):(\\d+)\\)$", "file": 3, "line": 4,
              "collapse": ["^(java|jdk|sun|kotlin\\.coroutines|org\\.junit|org\\.gradle|org\\.springframework)\\."] },
  "classify": [ { "regexp": "Could not find a valid Docker environment", "class": "environment", "note": "docker unavailable" } ],
  "outputs": ["build/", ".gradle/", "*/build/"], "executes_project_code": true }
```
`gradle:test-debug` is the same action with `--debug-jvm` and `"ready": "Listening for transport dt_socket at address: (\\d+)"`.

**Workspace manifest** (developer layer, `~/.config/itse/workspaces/crm.json`; `repo` is the repo key of §3.1):
```json
{ "itse": 1, "repo": "/work/crm/.git",
  "use": { "jdk": { "version": "25.0.5+8" }, "kotlin": {}, "gradle": {}, "node": { "install": "path" } },
  "system": { "libheif": { "lib": "libheif.so.1" }, "docker": { "cmd": ["docker","info"] } },
  "servers": { "kotlin-lsp": { "idle_minutes": 60 }, "some-mcp": { "expose": true, "tools": ["search", "fetch"], "max_list_bytes": 900 } },
  "inputs": [ { "id": "target", "type": "promptString" }, { "id": "filter", "type": "promptString" } ] }
```
- **`itse use <ext> [key=value…]`** adds or updates `use.<ext>` in the repo's developer-layer file, creating it (0600, dir 0700) with `repo` set; **`itse unuse <ext>`** removes the key, and the file when it is empty. Both write only that file, start nothing, and never change trust. Values are strings, `true`/`false` or numbers (`itse use jdk version=25.0.5+8`, `itse use node install=path`). Both run locally, without the daemon, and are in the MCP `test_build_run` tool (§7.4); an MCP agent can declare, but only a human can trust.
- `system` checks are typed, never shell strings. A failed check is `blocked` with reason `needs_image_layer` and actor `halti-owner`.
- `node: path` uses the image's Node and shows `unpinned (found …)`.
- All JSON models use `deny_unknown_fields`. The team layer has no `usage` key; an unknown key is an error that names the file. One exception keeps tasks.json entries pasteable verbatim: VS Code's UI keys (`presentation`, `runOptions`, `detail`, `icon`, `hide`, `promptOnClose`, `isBackground`, `windows`, `osx`, `linux`) are accepted and ignored, and `ext show` notes `ignored: presentation, runOptions`. A doc test parses one real tasks.json entry.
- Templates: `${pkg:x}`, `${consent:x}`, `${workspaceFolder}`, `${input:x}`, `${cache}`, `${target_args}`, `${filter_args}`, `${ready:N}`. Unknown keys are rejected at load. A missing required input is `ERROR test — gradle:test needs a target (inputs: target)`. An empty `${input:filter}` drops its `filter_args`.

**Lock:**
```json
{ "itse_lock": 1,
  "packages": { "kotlin-lsp": { "purl": "pkg:generic/jetbrains/kotlin-lsp@271.2085.0", "install": "fetch",
      "targets": { "linux_arm64": { "url": "https://download.jetbrains.com/language-server/kotlin-server/271.2085.0/kotlin-server-271.2085.0-aarch64.tar.gz",
                                    "sha256": "877aee61…dc96e", "size": null, "provenance": "upstream:openvsx-bundle" } },
      "published": "2026-11-04", "expires_estimate": "2026-12-04", "extension": "kotlin@0.1.0" } },
  "project_pins": { "gradle-wrapper": { "distributionSha256Sum": "present" } } }
```
I1 locks the current target only. `itse lock --target linux_x64` adds another. The lock is per repo (§3.1). If the repo has a team lock (`.itse/itse.lock.json`), its pins seed packages the machine lock doesn't have yet, and `status` shows where the two differ (open point O9).

### 4.4 Resolve

Resolving reads files only. It merges layers, runs detection, reads project pins, maps `uname -m` to a target, expands `requires`, renders templates, and gives each item one state:

| State | Meaning | Actor and next |
|---|---|---|
| `suggested` | detected from a file, not in any layer's `use` | agent: `itse use rust` |
| `ready` | installed (or found), pinned, consented, trusted if needed | – |
| `missing` | declared, not in the store | agent: `itse install <pkg>` |
| `unpinned` | `path` install found without a pin | – (shown as a note) |
| `untrusted` | runs project code; the workspace isn't trusted | human: `itse trust` |
| `needs_consent` | a declared consent isn't recorded | human: `itse consent <pkg>` |
| `expiring` | `expires_estimate` within 7 days | agent: `itse lock --refresh <pkg>` |
| `expired` | past the estimate, or the server exited with a mapped `server_expired` | agent: `itse lock --refresh <pkg>` |
| `blocked` | a system check failed, or the door refused a host | halti-owner |

The session host starts only `ready` items. Any other state is returned as the result item, with its actor and next step. A refresh of an expiring pin never changes trust (§10), so the agent's `lock --refresh` can't turn into a `needs_human` in the middle of a task. On a fresh repo with nothing declared, `status`, `test`, `problems` and `find` all return the `suggested` rows with their `next`, never an empty answer.

### 4.5 Install, verify, extract

1. If the store has the sha256, link it and stop.
2. **Fetch** (the only byte path): `curl -fsSL --proto =https -w '%{http_connect} %{http_code}' -C - -o ~/.local/share/itse/dl/<sha>.part <url>`, env inherited (HTTPS_PROXY, CURL_CA_BUNDLE). Downloads resume. They stage on persistent disk, not /tmp. If the `.part` already has the pinned size, it is verified before curl runs. On curl exit 33 (the server ignores Range) or an HTTP 416, the `.part` is deleted and the fetch is retried once without `-C -`.
3. **Verify** in-process: sha256 against the lock. On a mismatch the file is deleted and the result is `sha256_mismatch`. With no pin, the install is refused unless policy allows TOFU (human-only).
4. **Extract** (delegated) into `store/<sha>.tmp`: tar.gz/xz with `tar --no-same-owner -x`; single `.gz` with `gzip -dc`; zip/vsix with `python3 -I -m zipfile -e`, then `chmod +x` on each `bin` entry. `.zst` is refused.
5. **Walk** in-core: reject absolute symlinks and any path or symlink escaping the tree, allow internal relative symlinks, apply `strip`, `chmod -R a-w`, atomic rename. At daemon start, leftover `store/*.tmp` dirs are removed and installs that were in flight are marked `gone`. `gc` restores `u+w` on a package's dirs before removing it.
6. **Door error classes** (from Halti proxy.rs and inspect.rs):

   | Door response | Result reason | Actor |
   |---|---|---|
   | CONNECT 403 | `blocked_by_door{host}` | halti-owner |
   | CONNECT 502 | `door_upstream_tls_failed{host}` | halti-owner |
   | in-tunnel 403 `blocked:writes` | `door_refused_write` | halti-owner |
   | 421 | `door_host_mismatch` | halti-owner |
   | 502 `blocked:encoded` | `door_masked_encoding` | halti-owner |
   | anything else | `delegate_failed{tool, exit, last_stderr_lines[3]}` | agent |

All delegates (curl, tar, gzip, python3, keytool) sit behind a `Delegate` trait with `Real` and `Fake`, in Halti's World style.

**Pin sources** (`itse lock --refresh [pkg]`): `github-release` (the asset `digest`: rust-analyzer, Temurin) and `openvsx-bundle` (`server-bundle.json` plus `files.sha256`). Both fetch metadata with curl GET through the door and write a lock diff. Nothing is installed by a refresh.

**Offline**: whatever is in the lock and the store works without network. `status --hosts` lists what would still be fetched, and from which hosts.

`install` runs in the daemon as a run (`run:N`), so MCP gets a handle at once and polls with `show run:N` (bytes done of total, per-package state).

### 4.6 Environment, shims and the JVM truststore (`env`)

**Environment composition**, in order. Never `env_clear` (Halti's box rules):
1. Base: for servers, the daemon's env, which is its starter's env minus `ITSE_*`, `CLAUDE_*` and `AI_AGENT` (§3.3); for actions, the caller's `hello.env`.
2. `PATH` with the package bin dirs in front.
3. The entry's `env`.
4. `JAVA_TOOL_OPTIONS`: truststore flags appended after Halti's proxy flags; flags already present are skipped.
5. `jvm_args` appended to `jvm_args_env` (e.g. `IJ_JAVA_OPTIONS`), or to `JAVA_TOOL_OPTIONS` if none is named.
6. `LANG=C.UTF-8` only if neither `LANG` nor `LC_ALL` is set.

**JVM truststore.** A user-space JDK lacks the door CA. Itse runs that JDK's `keytool -importcert -noprompt -file $NODE_EXTRA_CA_CERTS -storetype PKCS12 -storepass changeit -keystore ~/.local/state/itse/jvm/door-<sha(ca)>.p12` and rebuilds it when the CA file's hash changes. Every JVM process Itse starts or shims gets `-Djavax.net.ssl.trustStore=… -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=changeit`. The password is not a secret.

**Shims.** `~/.local/share/itse/bin/<name>` is a symlink to `itse`, which dispatches on argv[0]. The link target is the calling CLI's own path (sent in `hello` as `exe_path`), never the daemon's `current_exe()`, which may name a deleted file (§0.2). It finds the workspace from the cwd, composes the env, and execs the store binary. If the tool isn't managed in that workspace, it execs the next same-named binary on PATH. If it is missing, untrusted, expired or lacks consent, it prints one line and exits 127: `itse: kotlin-lsp 271.2085.0 not installed; run 'itse install kotlin-lsp' (needs hosts open-vsx.org, download.jetbrains.com)`. Putting the dir on the base image PATH is Halti's EMA-215. Itse's tests set PATH themselves.

`itse exec -- cmd…` and `itse env [--sh]` give the same env without shims. They run locally, without the daemon.

## 5. Session host and protocol clients

### 5.1 JSON-RPC framing and peers (`rpc`)

```rust
pub enum Framing { Lines, Headers }     // Lines: MCP and the daemon; Headers: LSP/DAP "Content-Length: N\r\n\r\n"
pub enum Dialect { Lsp, Mcp, Dap }      // id correlation, cancel and error shapes
pub fn read(r: &mut impl BufRead, f: Framing, max: usize) -> io::Result<Option<Value>>; // None = EOF
pub fn write(w: &mut impl Write, f: Framing, v: &Value) -> io::Result<()>;              // write + flush
pub trait Handler: Send + Sync {
    fn notify(&self, method: &str, params: Value);
    fn request(&self, method: &str, params: Value) -> Result<Value, (i64, String)>; // server→client; never blocks
    fn closed(&self, why: &str);
}
impl Peer {
    pub fn start(r: impl BufRead + Send + 'static, w: impl Write + Send + 'static, f: Framing, d: Dialect, h: Arc<dyn Handler>) -> Arc<Peer>;
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancel: &Cancel) -> Result<Value, RpcError>;
    pub fn notify(&self, method: &str, params: Value) -> io::Result<()>;
}
pub enum RpcError { Timeout, Cancelled, Closed(String), Remote { code: i64, message: String, data: Option<Value> }, Io(io::Error) }
#[derive(Clone, Default)] pub struct Cancel(Arc<AtomicBool>);
```

- **Headers**: names are case-insensitive, `Content-Type` is ignored, a missing length is an error, EOF mid-body is an error. `max` is 4 MiB for every peer. An oversized body is read and discarded in 64 KiB chunks, so the stream stays in sync; the request it answered fails with `response_too_large` and the peer stays usable. A Lines frame over 4 MiB is skipped up to its newline the same way.
- **Lines**: blank lines are skipped. A non-JSON line from a child goes to its server log (as Halti's `browse::Session`). The daemon socket answers non-JSON with -32700.
- **Reader thread**: responses go to `pending`; requests from the other side go to `Handler::request` and are answered at once (no deadlock); notifications and events go to `Handler::notify`. On EOF every pending request gets `Closed`, then `closed()`. Handler calls run inside `catch_unwind`; a panic there closes the peer as `crashed: handler panic` instead of leaving it hung. The Handler touches only its own `Live` state (§5.2), never a slot's start lock.
- **`request`** waits with `recv_timeout(min(left, 100 ms))` and checks `Cancel` between waits. On timeout or cancel it sends the dialect's cancel and drops the entry: LSP `$/cancelRequest {id}`; MCP `notifications/cancelled {requestId, reason}`; DAP `cancel {requestId}` only if `supportsCancelRequest`.
- **DAP dialect**: `{seq, type:"request", command, arguments}`; responses matched by `request_seq`; `success:false` becomes `Remote`; events go to `notify(event, body)`; reverse requests (`runInTerminal`, `startDebugging`) are refused with `success:false`.

### 5.2 Registry, handles and spawn (`host`)

The resolver hands the host a `Launch`:
```rust
pub struct Launch {
    pub id: String, pub protocol: Protocol /* Lsp|Dap|Mcp */, pub argv: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)>,
    pub limits: Limits, pub exit_codes: BTreeMap<i32, String>, pub settings: Value, pub init_options: Value,
    pub languages: BTreeMap<String, String>, pub idle_minutes: Option<u64>, pub timeout_s: Option<u64>,
    pub expose: bool, pub via: Option<DapVia>, pub hide_vars: Vec<String>, pub watch_client: bool,
}
pub struct Limits { pub nofile: Option<u64>, pub as_mb: Option<u64>, pub cpu_s: Option<u64>, pub oom_score_adj: i32 }
```

Each start re-resolves, so manifest edits apply at the next start. A changed launch hash shows in `status` as `config changed; 'itse stop lsp:x' to apply`.

- **Registry**: `Host { ws, slots: Mutex<BTreeMap<String, Arc<Slot>>>, runs, debug, procs: Mutex<Procs> }`. A slot has two parts:
  - `phase: Mutex<Phase>` plus a `Condvar` (`Stopped | Starting | Up(Arc<Peer>) | Crashed(..)`). The first caller sets `Starting`, **releases the lock**, spawns and runs `initialize`, then sets `Up` and notifies. Concurrent callers wait on the Condvar for one start instead of spawning twice. No lock is held while waiting on the peer.
  - `live: Arc<Mutex<Live>>`, owned by the reader's Handler: diagnostics, progress tokens, registrations, server status, the last 5 messages. Handlers lock it briefly and never block.
  - `status` reads `phase` with `try_lock` and `live` snapshots, so it never hangs on a start and shows `starting (41 s)`.
- **Handles**: `lsp:<server-id>` and `mcp:<server-id>` (stable across restarts), `dbg:N`, `run:N`, `q:N` (counters in `next.json`, never reused).
- **`procs.json`**, written atomically (tmp + rename) on every change: `[{handle, kind, pid, pgid, start_time, argv0, started_at, log}]`.
- **Spawn**: `Command` with argv; `process_group(0)`; `pre_exec` calls only `setrlimit` (`RLIMIT_CORE=0` always; `NOFILE`, `AS`, `CPU` only when declared); stdin and stdout piped; stderr appended to `servers/<id>.log`. After spawn the parent writes `oom_score_adj`: 800 for servers, 500 for runs.
- **Memory**: `RLIMIT_AS` doesn't suit JVMs or rust-analyzer, so JVM memory is capped with `jvm_args` (`-Xmx2g`). With `oom_score_adj`, restartable servers die before the agent does. cgroups are not available to uid 1000 (§0.2).
- **`sys`** declares `kill`, `setsid`, `setrlimit`, `getuid`, `signal` and `mallopt` by hand with Linux constants and a `// SAFETY:` note each. `proc_stat(pid)` parses `/proc/<pid>/stat` after the last `)`: field 3 (state; `Z` and `X` count as dead) and field 22 (start time). `kill_group(pgid, grace, reap)` sends SIGTERM to `-pgid`, polls, then SIGKILL; `reap` is a non-blocking wait on the leader when it is our child, since an unreaped zombie keeps the group alive for the whole grace.

### 5.3 Crashes and daemon restarts

- **Crash**: an EOF Itse didn't cause makes the slot `crashed {exit, signal, mapped, stderr_tail[5]}`. `mapped` comes from `exit_codes` (`7 → server_expired`). An unrequested SIGKILL reads `killed by signal 9 (likely out of memory)`. Pending requests fail with `server_crash`.
- **No supervision**: nothing restarts in the background. The next call that needs the server starts it and says so: `restarted after crash (exit 137)`. A death before `initialize` completes is `start_failed` and is not retried in the same call.
- **Daemon restart**: the new daemon reads `procs.json`. A process counts as alive only if its `start_time` matches (PID reuse guard) and its state is not `Z` or `X`. For runs, `run.json`'s `exit` is the main signal. When a group leader is gone, `kill(-pgid, 0)` still checks for leftover members (cargo children of rust-analyzer), and they are killed if the recorded pgid matches.
  - Runs are re-adopted (§5.6).
  - stdio servers and adapters can't be re-adopted (their pipes died). Leftovers get `kill_group`. The handle shows `gone: daemon restarted`, and the next use starts cold (`index: cold`).
  - Debug sessions show `gone`, keeping their last snapshot id.
- **What survives killing the daemon**: results and runs on disk, run exit codes, handle names and counters, the last debug snapshots, the usage log, and the MCP connection (through the relay, §7.2). **What is lost**: warm indexes and live debug sessions, each flagged in results (open point O2).

### 5.4 LSP client (`lsp`)

**`initialize`** (timeout 120 s; `processId` = daemon pid; `rootUri` and `workspaceFolders` = root; `clientInfo {name:"itse", version}`; `initializationOptions` = `init_options`):
```json
{"general":{"positionEncodings":["utf-8","utf-16"]},
 "window":{"workDoneProgress":true,"showMessage":{}},
 "workspace":{"configuration":true,"workspaceFolders":true,"didChangeWatchedFiles":{"dynamicRegistration":false},
              "diagnostics":{"refreshSupport":true},"executeCommand":{},"symbol":{"symbolKind":{"valueSet":[1,2,…,26]}}},
 "textDocument":{"synchronization":{"didSave":true},"publishDiagnostics":{"relatedInformation":true,"versionSupport":true},
   "documentSymbol":{"hierarchicalDocumentSymbolSupport":true,"symbolKind":{"valueSet":[1,…,26]}},
   "hover":{"contentFormat":["plaintext","markdown"]},"definition":{"linkSupport":false},"references":{}},
 "experimental":{"serverStatusNotification":true}}
```
Then `initialized` and `workspace/didChangeConfiguration {settings}`. Shutdown: `shutdown` (5 s), `exit`, 2 s, `kill_group`.

**Requests from the server:**

| Request | Answer |
|---|---|
| `workspace/configuration` | one value per `items[].section`, by dotted path in `settings`; `null` if absent |
| `window/workDoneProgress/create` | `null`; the token is tracked |
| `client/registerCapability`, `unregisterCapability` | `null`; recorded for `status` |
| `workspace/{diagnostic,semanticTokens,inlayHint,codeLens}/refresh` | `null` |
| `workspace/workspaceFolders` | `[{uri, name}]` |
| `window/showMessageRequest` | `null`; logged |
| `window/showDocument` | `{success:false}` |
| `workspace/applyEdit` | `{applied:false, failureReason:"itse applies no edits in I1"}` |
| anything else | -32601 |

**Notifications**: `publishDiagnostics` stored per URI as compact structs (`{version, seq, items: [{range, severity, code, source, message}]}`, messages clipped to 512 B, at most 2,000 items per server, the excess counted in `status`); `$/progress` begin/report/end; `experimental/serverStatus` (`quiescent`, `health`); log and show messages go to the server log, the last 5 kept for `status`.

**Sync from disk before every query.** Agents edit files outside Itse.
- *Opened documents* (an LRU of 50): `stat` (mtime ns, size, inode) against what was sent. Changed: full-text `didChange` (version+1), then `didSave`. Deleted: `didClose`.
- *Unopened documents*: one `git --no-optional-locks status --porcelain=v1 -z --untracked-files=all` (also `GIT_OPTIONAL_LOCKS=0` in its env), at most once a second. Without `--no-optional-locks`, `git status` takes `.git/index.lock` and rewrites the index, which would write into the repo and race the agent's own git. For changed files of the server's filetypes with a new mtime, `didSave`, at most 50. If `HEAD` moved, one `didSave` for the newest changed file. Without git there is no sync of unopened files; servers that watch the disk themselves (rust-analyzer) still see them, and results say `no git: unopened files not synced`.
- Itse advertises no file watching, so servers watch the disk themselves (rust-analyzer does). `"watch": "client"` on a server entry also sends `workspace/didChangeWatchedFiles` (for kotlin-lsp or jdtls if they need it).
- Documents are opened only for queries that name a file or position. Files over 4 MB are refused (`file_too_large`). Bytes are read as lossy UTF-8. Positions are converted when the server chose utf-16.

**Settle and the cold flag.** `settle(deadline)` waits on a Condvar woken by the reader until: no progress token is active; `serverStatus.quiescent` is not false; 300 ms passed with no message; and, if anything changed, a push arrived for a changed URI or 1.5 s passed. At the deadline the result says `settled: false`. Every LSP result carries `index`: `cold` until the first settle after `initialize`, `busy` while progress is active, `warm` otherwise, plus progress text such as `Indexing 340/1200 (28%)`.

**Queries** (timeout 30 s):

| Verb | Requests |
|---|---|
| `find` | `workspace/symbol {query}` |
| `refs` | resolve the name via `workspace/symbol`; 0 or several matches return candidates; otherwise `didOpen` + `textDocument/references {includeDeclaration:false}` |
| `outline` | `textDocument/documentSymbol` |
| `info` | resolve as for `refs`, then `textDocument/hover` and `textDocument/definition` |

**Untrusted workspaces start no server that runs project code.** rust-analyzer's flycheck runs `cargo check`, which compiles and runs `build.rs` and proc macros, so `ext/rust` marks the rust-analyzer server `executes_project_code`, like kotlin-lsp. Before `itse trust`, `problems` and the nav verbs return `UNTRUSTED problems — rust-analyzer runs build scripts and cargo check; next (human): itse trust`. One rule, matching §4.4; a limited untrusted mode (`checkOnSave: false`, build scripts and proc macros off, no `didSave`) is I2 if usage shows the wait for trust matters.

**rust-analyzer settings in `ext/rust`**: `cargo.targetDir` = `${cache}/rust-analyzer/<ws>`, so its `cargo check` writes nothing in the repo and never holds the build-dir lock the agent's own `cargo` needs. The cost is a second build of dependencies on the first check (disk and minutes on a big crate). A developer who prefers sharing `target/` sets `servers.rust-analyzer.settings` in the developer layer without `cargo.targetDir`.

### 5.5 DAP client (`dap`)

**Launch modes** (the entry's `via`):
- `program`: spawn the adapter; DAP over stdio, Headers framing.
- `tcp {argv, port}`: spawn the adapter with a declared port and connect. (Scanning stdout for a port is cut from I1.)
- `lsp_command {server, command, arguments}`: `workspace/executeCommand` on `lsp:<server>`; the result is accepted as `n`, `"n"`, `{port}` or `"host:port"`.

TCP modes connect only, only to 127.0.0.1 or ::1 (refused in code otherwise), retrying 50 × 100 ms, and never listen. This needs the owner's reading of decision 2 (open point O1). If the owner wants decision 2 literally, the TCP leg is delegated instead: Itse spawns `python3 -I -c <bridge>` (a 15-line stdlib stdio↔TCP copier shipped as extension data in `ext/jdk`) and speaks DAP over its stdio, so the core opens no TCP socket. Either way the `dap` module sees only a byte stream; the transport is chosen after the JVM probe (§13, P-JVM), before S5.

**Handshake**: `initialize {clientID:"itse", clientName:"Itse", adapterID, pathFormat:"path", linesStartAt1:true, columnsStartAt1:true, supportsVariableType:true, supportsRunInTerminalRequest:false, supportsStartDebuggingRequest:false, locale:"en"}`, 10 s. Then `launch`/`attach` is sent **without waiting** (java-debug-style adapters answer only after `configurationDone`), then wait for the `initialized` event, then `setBreakpoints` per source (with `logMessage` when `supportsLogPoints`), `setExceptionBreakpoints` (the stop rule below), `configurationDone`, and finally wait up to 60 s for the `launch`/`attach` response.

**Default stop rule for `debug test`** (no `--at` given). A JUnit assertion failure is thrown in test code and caught by the framework, so an `uncaught` filter alone never stops. In order:
1. If the newest result of the same test target holds a failing `T` item (or the call names one: `debug test run:14/T1`), a line breakpoint at that item's first project frame.
2. Else exception breakpoints on **caught** exceptions limited to project code: through `exceptionOptions` or the filter's `condition` when the adapter advertises `supportsExceptionOptions` or `supportsExceptionFilterOptions` (the class or package prefix comes from the extension's `debug.project_packages`, else from the newest result's frames), plus `uncaught`. An adapter with only a plain `caught` filter gets it, and a stop whose top frame is not a project frame is continued automatically, at most 20 times (counted in the result).
The first line states the rule used: `STOPPED … — stop on: breakpoint at T1's frame OrderRepositoryTest.kt:42` or `stop on: caught exceptions in com.acme (4 framework stops skipped)`. Logpoints (`--log L=text`) never stop.

**Events** are queued per session on a Condvar: `stopped`, `continued`, `exited`, `terminated`, `output` (ring of 50 lines). `wait_stop(timeout)` (default 120 s) returns on stopped, terminated or timeout.

**Snapshot at a stop** (at most 50 requests, 5 s each, 10 s total):
1. `threads`, then `stackTrace {threadId, levels: 40}`.
2. Project frames (source under the root, not under `build/` or `target/`, `file://` URIs suffix-matched) are kept; the rest collapse to a count.
3. For the top 3 project frames: `scopes` (skipping `expensive`), then `variables`: depth 1, depth 2 for `this` and the exception; ≤ 20 per scope, ≤ 10 children, values clipped to 160 chars; names matching `hide_vars` dropped.
4. `exceptionInfo` when the reason is `exception` and supported.
5. Source window ±2 lines, read from disk.

The full snapshot is stored as `debug/<n>/s<k>.json`. The text shows frame 1's locals; past 4,000 chars it drops children, then later frames, and sets `truncated`.

**Other operations**: `continue`; `next`/`stepIn`/`stepOut` with `granularity: "line"`, each returning a new snapshot; `evaluate {expression, frameId, context:"repl"}`, clipped; `disconnect {terminateDebuggee: launch}`. Adapter EOF marks the session `gone`. Idle reap after 10 minutes.

### 5.6 Long actions (`actions`)

Actions are resolved VS Code task entries. `type: process` uses the argv; `type: shell` runs `/bin/sh -c` on the command line joined with VS Code quoting. `dependsOn` runs sequentially.

They run through a re-exec'd runner, so **a run outlives the daemon and its exit status is never lost**:
1. The daemon writes `runs/<n>/run.json` = `{spec: {steps: [[argv, cwd]…], limits, label, started_at, deadline_ms}}`: the `dependsOn` steps in order, then the action's own, so one runner runs them all and the first that fails ends the run; `deadline_ms` (from `timeout_s`, in ms since the epoch) lets a re-adopting daemon keep the timeout. The env is not written.
2. It spawns `/proc/self/exe __run <rundir>` with `setsid` in `pre_exec` and the composed env on the `Command`. `/proc/self/exe` still works after the binary is replaced; `current_exe()` does not (§0.2).
3. The runner (about 40 lines) is the group leader, catches SIGTERM/SIGINT/SIGHUP into a flag, spawns the action with stdout and stderr on one fd appended to `out.log` (terminal-like interleaving), rewrites `run.json` atomically with `pid {runner_pid, child_pid, start_time}` before each step, waits, then rewrites it with `exit {code, signal, cancelled, timed_out, ended_at, ms}`. One small record per run instead of three files.
4. A daemon thread waits on the runner. A re-adopted run is polled every second for `exit` and for liveness (start time, not a zombie).
5. If the runner ended with no `exit`, the daemon writes the runner's own status when it was its child (cancelled only if the daemon cancelled it), else `{signal: 9, cancelled: true}`. A cancel that reaches the runner between steps starts no further step.

**Cancel and timeout** use one escalation: SIGINT to the child; after 5 s SIGTERM to the group; after another 5 s SIGKILL. A Gradle daemon started inside the group dies at step 2 (open point O11).

**Waiting**: `wait(run, Exit | Pattern(regex), timeout)` tails `out.log`. The pattern form returns captures as `${ready:N}`. Parsers run after exit (§6.4).

**Not in the core**: restart, keep-alive and supervision policies (EMA-193). Concurrent runs of one label are allowed; the result names any other live run of it. `install` also runs in the daemon process (not through `__run`), so a daemon crash ends it; the next start cleans its `*.tmp` dirs (§4.5).

`debug test` in one call: start the extension's debug action (`gradle:test-debug`) as `run:N`; wait for `ready`; start or connect the adapter; `attach` with templated arguments (`${ready:1}`); breakpoints (`--at`, `--log`, else the default stop rule of §5.5); run to the first stop; return the snapshot. No stop: `EXITED … — no stop (stop on: caught exceptions in com.acme); test result in run:N`.

### 5.7 Hosted MCP servers, client side (`mcp/client.rs`)

- stdio, Lines framing, Mcp dialect. `initialize {protocolVersion: "2025-06-18", capabilities: {}, clientInfo: {name: "itse", version}}` (30 s). The server's version must be one of `2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25`, else `mcp_version_unsupported`. Then `notifications/initialized`.
- `tools/list` follows `nextCursor` up to 200 tools. The list is cached in state keyed by the pin's sha256 (or the found binary's), so later `tools/list` calls to Itse don't cold-start the server. `notifications/tools/list_changed` re-lists.
- **First listing.** With no cache yet, Itse's own `tools/list` starts the exposed server and waits up to 10 s for its list. If it is late, Itse lists the core tools only, and sends one `notifications/tools/list_changed` to the harness when the cache fills (Itse advertises `listChanged: true`; whether Claude Code re-lists is checked in the S1 probe). `status` says `some-mcp: tools not listed yet (first start)`.
- **What is exposed** is the developer's choice per server: `tools: [names]` (an allowlist; default all) and `max_list_bytes` (default 1,200). Each exposed description is clipped to its first sentence, at most 160 chars; input schemas pass unchanged. The whole list, core tools plus exposed tools plus `instructions`, must fit 3,600 bytes (~1K tokens, EMA-188). Tools that don't fit are dropped in server order and named in `status`: `some-mcp: 9 of 14 tools dropped (list cap 3,600 B); set tools: [...]`.
- `tools/call {name, arguments}` passes through (120 s, or `timeout_s`). Server `ping` gets `{}`. Sampling, elicitation and roots get -32601.
- Tools are named `<server>__<tool>` with `-` mapped to `_`. Names over 64 chars are skipped with a `status` note (Halti's `MAX_TOOL_NAME`).

| I1 | I2 |
|---|---|
| this client: start on demand, warm, idle stop, crash handling as §5.3 | several exposed servers, `list_changed` forwarded to harnesses |
| servers with `"expose": true` in the developer layer (off by default) listed in Itse's `tools/list` | resources, prompts, Tasks passthrough |
| `itse ext tools <server>` (CLI); exposed tools are called directly over MCP | `ext call` on the CLI, router or lazy listing, schema trimming, registry `server.json` |

## 6. Verbs and result shapes

### 6.1 The verb table (`verbs`)

Every verb is one row of `const VERBS` in `src/verbs.rs`. The table drives the CLI parser, the MCP tool enums, the generated index (`itse verbs`) and the usage log's closed verb set.

```rust
pub struct Verb { pub name: &'static str, pub tool: Tool, pub params: &'static [Param], pub runs: Runs,
                  pub human_only: bool, pub milestone: Milestone, pub about: &'static str }
pub enum Tool { Query, Run, Show, Debug, CliOnly }   // which MCP tool carries it (names in §7.4)
pub enum Runs { Daemon, Local }                       // Local: exec, env, stats, verbs, integrate, use, unuse
pub enum Milestone { I1, Reserved(&'static str) }     // Reserved("EMA-210")
```

| Verb | Arguments | Backed by | Result |
|---|---|---|---|
| `status` | `[--hosts]` | resolver, registry | one state row per item, `suggested` rows for detected extensions; MCP list size; usage-log line; with `--hosts` the hosts the plan needs |
| `use` | `<ext> [key=value…]` | developer layer (local) | the changed `use` entry and the new state rows |
| `unuse` | `<ext>` | developer layer (local) | the removed entry |
| `install` | `[pkg…]` | resolver fetch/path, as a run | `run:N`; per-package state; bytes done of total |
| `lock` | `[--refresh [pkg]] [--target T]` | pin sources | lock diff; nothing installed |
| `trust` | `[--revoke]` | trust record (human-only) | state row |
| `consent` | `<pkg> [--eula-hash H] [--region R] [--data-sharing V]` | consent record (human-only) | state row |
| `ext` | `list`, `show <id>`, `tools <server>`, `add <file>`, `remove <id>` | extension loader; `add`/`remove` human-only | rows or declared contents |
| `env` | `[--sh]` | env composition (local) | key=value lines |
| `exec` | `-- cmd…` | env composition, spawn, wait (local) | the child's exit status |
| `gc` | `[--keep-locked]` | store and results store | bytes freed |
| `run` | `<label> [--input k=v…] [-- args]` | declared action | exit status, parsed items, output tail |
| `build` | `[target] [--action L]` | action with `group.kind == "build"` | located problems |
| `test` | `[target] [filter] [--action L]` | action with `group.kind == "test"` | failing tests with trimmed traces |
| `problems` | `[scope] [--new] [--all]` | LSP diagnostics and the newest action problems | located problems with source lines |
| `find` | `<name> [--kind K]` | `workspace/symbol` | matches with the source line |
| `info` | `<name\|S-id>` | `workspace/symbol`, `hover`, `definition` | signature, docs, location |
| `refs` | `<name\|S-id>` | `workspace/symbol`, `references` | references grouped by file |
| `outline` | `<path>` | `documentSymbol` | symbol tree, depth ≤ 2 |
| `debug` | `test <target> [filter\|run:N/T1] [--at L…] [--log L=text…]`, `attach <host:port> [--at…]`, `step <dbg> [over\|in\|out]`, `continue <dbg>`, `eval <dbg> <expr> [--frame F]`, `snapshot <dbg> [--frame F]`, `stop <dbg>` | DAP | one-call snapshot |
| `show` | `<id> [--page N] [--raw] [--grep RE] [--tail N] [--json]` | results store | full output, paged |
| `stop` | `<handle>` | LSP `shutdown` / MCP stdin close / DAP `disconnect` | state row |
| `cancel` | `<run:N>` | signal escalation (§5.6) | the run's final state |
| `stats` | `[--since D] [--by K] [--json]` | usage log (local, CLI only, not counted) | §8.4 |
| `verbs` | `[--md\|--json]` | the verb table, declared actions, extension skills text | the generated index |
| `integrate` | `<ext> [--skill] [--mcp] [--recipe R] [--remove] [--home DIR] [--yes]` | harness extension data (local, CLI only, human-only) | files changed and commands run |
| `hook` | `<recipe>` | `problems --new` on live servers only, wrapped as declared | hook JSON or nothing |
| `mcp` | – | stdio relay | – |
| `daemon` | `stop`, `restart`, `status` | the workspace daemon (CLI only) | state row |
| `rename`, `fix` | – | reserved (EMA-210, I2) | `RESERVED`, exit 2 |

The internal entries `__run`, `__daemonize` and `daemon --root` (the daemon itself) are CliOnly rows too, hidden from the index.

Rule (EMA-206): a new verb needs a protocol request or an action behind it, and a measured reason (§8). `stop` and `cancel` are backed by protocol shutdown and process signals; `use` and `unuse` by the manifest.

**Human-only** (`trust`, `consent`, `ext add/remove`, `integrate`, accepting a TOFU pin): over MCP they return `NEEDS_HUMAN` with the CLI line to run in a terminal. On the CLI they are advisory in I1 and run; the record stores `actor: "tty"` or `"non-tty"`. Over MCP, `ext` accepts only `list`, `show` and `tools`.

**Exit codes**: 0 ok, or empty for listings (`problems`, `status`, `show`); 1 fail (the verb worked and reports failure), including a name lookup (`find`, `info`, `refs`) that found nothing, which is useful in scripts; 2 error; 3 `needs_human`, `needs_consent`, `untrusted` or `blocked`; 4 running (detached); 130 a second Ctrl-C. Shims use 127.

**Arguments** are parsed with lexopt into `Args { target, rest, flags }`. Every MCP tool takes the same tokens as `args?: string[]`, parsed by the same function, so every CLI flag works unchanged over MCP (`{"verb":"problems","args":["data/","--all"]}`); `every_cli_flag_is_reachable_over_mcp` walks `VERBS` and checks it.

**Choosing the action** for `test` and `build`: the one with `group: {kind, isDefault: true}` among used extensions, or the one named with `--action`. Several defaults (rust and gradle in one repo) give `ERROR test — 2 default test actions: cargo:test, gradle:test (--action L)` with both as candidates. Picking by the target's file types is I2.

### 6.2 One result schema (`results`)

```rust
pub struct Outcome { pub verb: &'static str, pub verdict: Verdict, pub scope: String, pub counts: String, pub cost: Cost,
                     pub id: Option<String> /* run:14, q:88, dbg:2 */, pub items: Vec<Item>, pub states: Vec<State>,
                     pub notes: Vec<String>, pub full: Size, pub page: Page }
pub enum Verdict { Ok, Fail, Error, Empty, Running, Stopped, Exited, NeedsHuman, NeedsConsent, Untrusted, Blocked, Unready, Reserved }
pub struct Item { pub id: String /* E1 W2 T3 S1 R4 F1 */, pub kind: &'static str, pub loc: Option<Loc>, pub src: Vec<(u32, String)>,
                  pub msg: String, pub detail: Vec<String>, pub sources: Vec<String> }
pub struct Loc { pub path: String /* repo-relative, else ~/… */, pub line: u32, pub col: u32 }
pub struct State { pub item: String, pub state: &'static str, pub reason: Option<String>, pub actor: Option<&'static str>,
                   pub next: Option<String>, pub hosts: Vec<String>, pub detail: Option<Value>, pub job: Option<String> }
pub struct Cost { pub ms: u64, pub warm: Option<bool> }
pub struct Page { pub n: u32, pub of: u32 }
```

`--json` prints `{"itse":1, …Outcome}`. `State` is the ecosystem's per-item row; `actor` is `agent`, `human` or `halti-owner`; `next` is a command, not advice. `next` is stored as verb plus args and rendered for the caller: `itse lock --refresh kotlin-lsp` on the CLI and hook, `test_build_run {verb: "lock", args: ["--refresh", "kotlin-lsp"]}` over MCP; human-only steps always render as the CLI line plus `in a terminal`. These types are about 60 lines after rustfmt, which put every field on its own line; the caps of §11.2 count them that way.

**Rendering rules:**
1. First line: `VERDICT verb scope — counts; cost; id`. Example: `FAIL test :data — 2 failed, 141 passed; 38.2 s, warm; run:14`.
2. Item ids are short and unique in one result. Every pointer Itse prints is fully qualified (`itse show run:14/T1`), so it can be followed from any caller. A bare `show T3` resolves against the newest result of the same session key that holds a T3 (§3.4), else the newest result in the workspace that holds one, and names the result it picked.
3. Locations are `path:line:col`, repo-relative, followed by the source line as `NN | text`, read from disk, clipped to 160 chars, tabs as 4 spaces.
4. Caps: verbs 40 lines or 4,000 chars per page; `show` 200 lines or 16,000 chars; `hook` 2,000 chars. An item is never split across pages. Details: 3 message lines, 6 kept frames, 1 line per cause.
5. Pages: the last line is `page 1 of 3; full: itse show run:14 --page 2`, or `full: itse show run:14/T1` when items hold hidden detail. JSON pages hold the same items as text pages. A page past the end is `ERROR show run:14 — page 4 of 3`.
6. Wording: core templates are in one `const TEXT` table. A test rejects `please|you should|you must|make sure|don't|do not|always|never` in it. Server and compiler messages pass through verbatim.
7. Freshness is stated: `kotlin-lsp index cold (3/14 modules, 41 s)`, `from run:12, 6 min old, 2 files changed since`, `report written before this run`.
8. Noise lines (`^Picked up JAVA_TOOL_OPTIONS`) are dropped by `drop_lines` regexes from extension data, never hard-coded.

**Results store**: actions in `runs/<n>/` (`result.json` next to `out.log`), queries in `results/<n>.json`, debug snapshots in `debug/<n>/`. Kept: the newest 200 results or 512 MiB per workspace, whichever is smaller; `gc` removes more.

### 6.3 Status

```
UNREADY status ~/work/crm — 4 of 8 not ready; untrusted; layer from repo /work/crm, team layer .itse/ read-only
ready          jdk 25.0.5+8          store sha256-bb5b2ded…
ready          gradle (project)      gradlew 9.8.0; distributionSha256Sum present
missing        kotlin-lsp 271.2085.0 not in store; 412 MB; hosts open-vsx.org, download.jetbrains.com
                                     next (agent): itse install kotlin-lsp
needs_consent  kotlin-lsp            eula, region, data_sharing; next (human): itse consent kotlin-lsp
untrusted      workspace             gradle:test and kotlin-lsp run project code; next (human): itse trust
blocked        docker (system)       docker info exit 1; needs_image_layer docker; actor halti-owner
suggested      node                  package.json found; next (agent): itse use node
running        none
note           kotlin-lsp            another kotlin-lsp runs for this root outside itse (pid 3121); two use ~2x memory
daemon         pid 4242, started by mcp 14 min ago; 0.1.0
mcp tools: 3 (~520 tokens incl. instructions; list 1,870 of 3,600 B); debug tool off (mcp.debug: true adds ~210 tokens)
usage log: on, ~/.local/state/itse/usage.jsonl (4.1 MB); ITSE_USAGE=off or "usage": false in ~/.config/itse/config.json stops it
```
A missing language server is reported, never hidden (EMA-195). The `note` about an outside server comes from a scan of `/proc/*/cmdline` and `/proc/*/cwd` for a declared server's binary name with the same root (Claude Code's own LSP plugins, a second editor). Itse takes no action; the developer picks one.

### 6.4 Tests, builds and problems

`test` and `build` pick the action as §6.1 says. All tool specifics are extension data.

**Gradle (JUnit XML):**
```
FAIL test :data (gradle:test) — 2 failed, 141 passed, 3 skipped; 38.2 s; run:14
T1 com.acme.data.OrderRepositoryTest > saves order with lines
   org.opentest4j.AssertionFailedError: expected: <3> but was: <2>
   data/src/test/kotlin/com/acme/data/OrderRepositoryTest.kt:42:9
     42 |         assertEquals(3, saved.lines.size)
   at OrderRepository.save  data/src/main/kotlin/com/acme/data/OrderRepository.kt:88
     88 |         lines.dropLast(1).forEach(::insert)
   … 61 framework frames
T2 com.acme.data.ShipmentQueryTest > finds by region
   org.testcontainers.containers.ContainerLaunchException: Container startup failed for image crewpointtech/postgis-pgvector:17-3.6-min
   Caused by: DockerClientException: Could not find a valid Docker environment
   class: environment (docker unavailable)
reports: 14 files written by this run; log 2,210 lines
full: itse show run:14/T1 · itse show run:14 --tail 80
```

**cargo (this box, through `ext/rust` matchers):**
```
FAIL test itse (cargo:test) — 1 failed, 212 passed, 4 ignored; 6.1 s; run:15
T1 mcp::tests::initialize_picks_the_newest_shared_version
   assertion `left == right` failed
     left: "2025-06-18"
    right: "2025-11-25"
   src/mcp/tests.rs:88:5
     88 |     assert_eq!(reply["protocolVersion"], "2025-11-25");
note: doctests not run (I2)
full: itse show run:15/T1
```
The `rust` built-in declares (checked on 1.92 output):
- `detect: ["Cargo.toml"]`; the `rust-analyzer` server (`install: path` in I1, `executes_project_code`, `cargo.targetDir` under `${cache}`, §5.4).
- `cargo:build` and `cargo:test` with `--message-format=short`, which prints `src/lib.rs:7:26: error[E0308]: mismatched types` on one line. `cargo:test` adds `--no-fail-fast` (like Gradle's `--continue`), so a failing lib test binary doesn't hide the integration tests. `outputs: ["target/", "Cargo.lock"]`.
- `$rustc-short`: a single-line matcher for that form.
- `$libtest-panic`: a fixed two-line matcher. Line 1 `thread 'NAME'( \(\d+\))? panicked at FILE:LINE:COL:` (the `(PID)` group is optional, since repos pin older toolchains that don't print it), line 2 the message, then up to 3 detail lines until a blank line. `drop_lines` removes `^note: run with .RUST_BACKTRACE` first, so it never fills a detail line. It makes a `T` item (Itse's matcher additions: `kind: "test"`, `name`, `detail_lines`). The golden test input is captured 1.92 output.
- `counts`: `test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored`, summed across test binaries.
- `--tests` leaves doctests out; they need a `loop` matcher (I2). Every cargo test result carries the fixed note `doctests not run (I2)`, so `all passed` is never claimed for them.

**JUnit XML scanner** (`parse/junit.rs`, no XML crate): a byte loop streamed through a `BufReader`, so memory doesn't grow with the file; only failure, error and skipped text is kept (≤ 8 KB per testcase), `system-out`/`system-err` are skipped; skips `<?…?>`, comments and `<!DOCTYPE…>`; takes CDATA as text; decodes `&lt; &gt; &amp; &quot; &apos; &#N; &#xH;`; tracks `testsuite → testcase → failure|error|skipped|system-out|system-err`. Only files with mtime ≥ run start − 1 s are read; if none are fresh, the newest stale ones are used and marked `report written before this run` (Gradle `UP-TO-DATE`). A malformed file is one `error` item (`unparsed report data/build/…/TEST-X.xml at byte 9120`); the others still parse.

**Trace trimming** (`parse/trace.rs`): keep line 1 and `Caused by:` lines; collapse frames matching `frames.collapse` into `… N framework frames`; keep the first frame of each cause; resolve a frame's file to a repo path by suffix in a lazily built file-name index (from `git ls-files`, else a walk skipping `.git`, `build`, `target`, `node_modules`; at most 50K names, `index_truncated` past that); `classify` adds a `class:` line.

**problemMatcher subset**: single-line patterns and fixed multi-line patterns (no `loop`); `regexp`, `file`, `line`, `column`, `severity`, `code`, `message`; `fileLocation` `absolute`/`relative` with `file://` stripped; named matchers in the extension's `problemMatchers`, referenced as `$name`. Patterns regex-lite can't compile (lookaround, backreferences) are refused when the extension loads. regex-lite's `\w`, `\d`, `\s` are ASCII-only; `docs/manifest.md` says so.

**SARIF** is I2 (no I1 extension produces it; open point O14).

**Problems**, merged and errors first:
```
FAIL problems data/ — 2 errors, 1 warning; kotlin-lsp live (index warm), gradle:compile run:12 6 min old
E1 error data/src/main/kotlin/com/acme/data/OrderRepository.kt:88:36 Unresolved reference 'insertt'. [kotlin-lsp]
     88 |         lines.dropLast(1).forEach(::insertt)
E2 error data/src/main/kotlin/com/acme/data/Region.kt:14:5 'when' expression must be exhaustive. [kotlin-lsp, kotlinc]
     14 |     when (r) {
W1 warning data/src/main/kotlin/com/acme/data/Mapper.kt:31:9 Variable 'tmp' is never used. [kotlinc]
     31 |     val tmp = row.get("id")
```
- Sources: the push-diagnostics cache of running servers in scope, after the disk sync and settle of §5.4 (deadline 20 s), plus the newest problems of each action run. A server that isn't running is not started by `problems` unless the scope's filetypes have a declared `ready` server; then it starts and says `index cold`. The hook path never starts one (§7.5).
- Fingerprint: sha256 of (tool, path, message with digits as `#`, trimmed line text). It holds when lines move. Duplicates merge, with sources listed.
- Order: severity, path, line. Hints only with `--all`.
- `--new`: the delta against the last set returned under the same baseline key, stored in `baselines.json`. The key is `<via>:<session key>` (§3.4): per Claude Code session for the hook (stdin `session_id`) and for the CLI (`CLAUDE_CODE_SESSION_ID` through `ext/claude-code`), `cli:cli` for a plain shell, per connection for MCP. So two subagents in one workspace don't swallow each other's errors. A result that is not settled doesn't advance the baseline. First line: `1 new error; 3 unchanged, 1 fixed since 14:02:10`.

### 6.5 Navigation by name

```
OK info OrderRepository.save — 1 match; kotlin-lsp index warm
S1 method com.acme.data.OrderRepository.save data/src/main/kotlin/com/acme/data/OrderRepository.kt:84:9
     84 |     fun save(order: Order): SavedOrder {
   fun save(order: Order): SavedOrder
   Saves the order and its lines in one transaction.
   refs S1: 12
```
- A query is split on `.`, `::` or `#`. The last segment must equal `SymbolInformation.name`. The segment before it must equal the last segment of `containerName` (rust-analyzer gives only the impl's self type, `Region`) or, when `containerName` is null or shorter, match a path component or file stem of the location (`data::Region.save` matches `src/data/region.rs`). Earlier segments match path components. With no exact match, case-insensitive prefix matches are tried. `--kind method` accepts kinds 6 (Method) and 12 (Function), since rust-analyzer reports methods as 12.
- Zero matches: `EMPTY find OrderRepo.svae — 0 matches; nearest: S1 OrderRepository.save, S2 OrderRepository.saveAll` (exit 1). Several: all are listed with S-ids, and `info S2` picks one.
- `refs` adds the note `precise references only; strings and comments not searched`.

### 6.6 Debug snapshot

```
STOPPED debug test :data OrderRepositoryTest — dbg:2 caught exception in OrderRepository.save, thread "Test worker"; stop on: caught exceptions in com.acme (2 framework stops skipped); attach+stop 11.4 s
data/src/main/kotlin/com/acme/data/OrderRepository.kt:88:9
     86 |         val saved = tx { insertHeader(order) }
     87 |         val lines = order.lines
  >  88 |         lines.dropLast(1).forEach(::insert)
     89 |         return saved.copy(lines = lines.size)
     90 |     }
locals
   order  Order            Order(id=O-17, region=EU, lines=[3 items])
   lines  List<OrderLine>  size=3
   saved  SavedOrder       SavedOrder(id=O-17, lines=0)
   this   OrderRepository  jdbc=JdbcTemplate@4f2a, clock=FixedClock
frames (project)
   F1 OrderRepository.save               data/src/main/kotlin/com/acme/data/OrderRepository.kt:88
   F2 OrderRepositoryTest.saves order…   data/src/test/kotlin/com/acme/data/OrderRepositoryTest.kt:41
   … 58 framework frames
more: debug step|continue|eval|snapshot|stop dbg:2
```

## 7. Exposure: CLI, MCP, skill and hook

### 7.1 CLI

- `itse <verb> …` connects to the workspace daemon (starting it), sends `hello` and one `call`, prints `text`, and exits with `exit`.
- `--wait` defaults to 100 s, under Claude Code's 2-minute Bash default. A run that outlives it returns `RUNNING test :data — run:14; 1 m 40 s so far; last line "> Task :data:test"` (exit 4). `show run:14` on a running run waits up to 30 s, then reports progress.
- Errors print as `itse: {e:#}`, lowercase, naming the object.

### 7.2 `itse mcp`: a stdio relay to the daemon

Serving MCP inside the harness's own process would tie warm servers to one harness. That breaks EMA-188's "warm across harness restarts". So `itse mcp` is a relay (Halti's `cred-shim mcp`, box/shim/src/main.rs:205-227), made line-aware:
- It sends `hello {mode: "mcp"}`, then copies lines both ways. It keeps the client's `initialize` line and the ids of requests in flight.
- On daemon EOF it answers each in-flight `tools/call` with a tool result, `{content: [{type: "text", text: "ERROR <tool> — itse daemon restarted; the call did not finish; call again"}], isError: true}`, and any other in-flight request with JSON-RPC -32603. It reconnects (auto-starting), replays `initialize` under id `"itse-replay"`, swallows the reply, and sends `notifications/initialized`. Client lines that arrive meanwhile are buffered (up to 1 MB, then the relay exits 1). After 3 failures in 60 s it exits 1 with one stderr line.
- If no daemon can start: `itse mcp: <error>` on stderr, exit 1.

In the daemon, `mcp::serve_io(reader, writer, conn)` is Halti's loop (mcp.rs:127-162). A `tools/call` runs on a worker thread, so `ping` and `notifications/cancelled` are answered during a long test. All writers share one `Mutex` around the connection's writer, so lines never interleave.

The relay is registered per root (local scope, §7.5), so it, and the daemon it starts, run only in harness sessions for a repo the developer integrated, not in every session on the machine.

### 7.3 Version negotiation

- S1: `const SUPPORTED: [&str; 2] = ["2025-11-25", "2025-06-18"]`. `initialize` echoes a supported request, otherwise answers `2025-11-25`, and the client decides. Reply: `capabilities: {tools: {listChanged: true}}` (used once, for a hosted server's first listing, §5.7), `serverInfo {name: "itse", version}`, and the two-line `instructions` of §7.4.
- After the week-1 probe confirms Claude Code uses it over stdio: the `2026-07-28` era. `server/discover` returns `{supportedVersions: ["2026-07-28", "2025-11-25", "2025-06-18"], capabilities, instructions}`; a modern request whose `_meta` version is unknown gets -32022 with `data.supported`; modern `tools/list` adds `ttlMs: 300000` and `cacheScope: "private"`. About 40 lines, from the reserve.
- No `structuredContent`: Claude Code shows the text block, so both would double the payload. JSON is `show <id>` with `json: true`.
- Long runs use polling (`run:N` plus `show_result`), which works with every client. MCP Tasks are I2 (open point O5); the line reserve no longer holds room for them.

### 7.4 Tool list

Claude Code 2.1.x defers MCP schemas behind tool search (§0.2): what the agent sees first is the tool names and the server's `instructions`. So the names and the first words of each description carry the keywords an agent searches for (test, build, problems, diagnostics, symbol, references, show), and the instructions are part of the budget.

Measured at chars/3.6:

| Shape | Tokens |
|---|---|
| one tool per verb (19) | ~1,715, over the cap |
| router `itse{verb, args}` | ~240, but argument docs move into the skill |
| **chosen: three tools** | **~430**, plus ~90 for `instructions` |
| chosen plus `debug` (opt-in) | ~640, plus instructions |

| Tool | Description starts with | Schema |
|---|---|---|
| `test_build_run` | "Run tests, builds and declared actions; install, lock, use." | `{verb: enum[test, build, run, install, lock, use, stop, cancel], target?: string, args?: string[], wait?: int}`; MCP `wait` default 50 s |
| `problems_symbols` | "Problems (compile errors, diagnostics), symbols by name (find, refs, info, outline), status." | `{verb: enum[problems, find, info, refs, outline, status, ext], target?: string, args?: string[], page?: int}` |
| `show_result` | "Show a stored result or log by id (run:14, run:14/T1): pages, grep, tail." | `{id: string, page?: int, grep?: string, tail?: int, json?: bool}` |
| `debug` | "Debug a test: stop, locals, stack, step, eval (DAP)." | `{verb: enum[test, attach, step, continue, eval, snapshot, stop], target?: string, args?: string[]}`; **listed only with `"mcp": {"debug": true}`** in `~/.config/itse/config.json` |

Each description then says that `args` takes the CLI's flags (`args: ["--new"]`, `args: ["--kind", "method"]`). The verb names inside the enums are the CLI's names, so the skill, the index and MCP use one vocabulary.

**`debug` is off by default.** R4 found Sonnet 4.5 lost 11 points with debug tools exposed directly, while calling them in 9% of cases. Debugging stays reachable through the CLI (`itse debug test …`) and the skill; `status` shows the token cost of turning the tool on. A DAP server being declared doesn't change the list.

**`instructions`** (in `TEXT`, counted in the list-size test):
```
itse runs this workspace's declared tests, builds and language servers and returns located, paged results with ids.
test_build_run: test, build, run <label>, install, lock, use, stop, cancel. problems_symbols: problems (compiler and LSP diagnostics), find, refs, info, outline (symbols by name), status, ext. show_result: run:14/T1, pages, grep, tail.
```

- A tool result is `{content: [{type: "text", text}], isError}`. `isError` is true only for the `error` verdict.
- States are not errors: `needs_human`, `needs_consent`, `untrusted` and `blocked` return `isError: false` with a first line such as `NEEDS_HUMAN test :data — workspace not trusted; next (human): itse trust in a terminal`.
- An unknown tool or bad arguments is `isError: true`, never a JSON-RPC error (Halti mcp.rs:267-276). `ext add` or `ext remove` over MCP is `NEEDS_HUMAN`.
- Exposed extension tools (§5.7) are appended as `<server>__<tool>`, counted in the usage `init` record. The whole list (core tools, exposed tools and `instructions`) is capped at 3,600 bytes when listed; `tool_list_with_instructions_stays_under_3600_bytes` covers the core with `debug` on, and `exposed_tools_past_the_cap_are_dropped_and_named` covers the rest. `status` shows the current size.

### 7.5 Verb index, skill and the Claude Code extension

`itse verbs` prints the index for the current workspace: the verb table (without reserved and human-only verbs), the declared actions by label, the used extensions' `skills[].text`, and `debug` when a DAP server is declared. `docs/verbs.md` is `itse verbs --md` for a workspace with only built-ins, golden-tested.

The `claude-code` built-in is data only:
```json
{ "itse_ext": 1, "id": "claude-code",
  "identity": { "session_env": "CLAUDE_CODE_SESSION_ID", "client_env": "AI_AGENT",
                "client_regexp": "^(claude-code)_(\\d+)-(\\d+)-(\\d+)_", "hook_session_from": "/session_id" },
  "integration": {
    "skill": { "path": "~/.claude/skills/itse/SKILL.md", "name": "itse", "description": "Workspace facts from itse: tests, problems, symbols by name, debug snapshots" },
    "mcp":   { "add": ["claude", "mcp", "add-json", "itse", "${json}", "--scope", "local"], "remove": ["claude", "mcp", "remove", "itse", "--scope", "local"],
               "cwd": "${workspaceFolder}", "value": {"type":"stdio","command":"itse","args":["mcp"]} },
    "recipes": { "post-edit": { "file": "~/.claude/settings.json", "pointer": "/hooks/PostToolUse", "append":
        {"matcher":"Edit|Write|MultiEdit","hooks":[{"type":"command","command":"itse hook post-edit","timeout":10}]} } } },
  "hooks": { "post-edit": { "path_from": "/tool_input/file_path", "cwd_from": "/cwd", "verb": "problems", "flags": {"new": true},
             "wrap": {"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"${text}"}}, "quiet_when": "empty" } } }
```

**`itse integrate claude-code --skill|--mcp|--recipe post-edit [--home DIR] [--yes]`** is the only writer of harness files, and it is human-only (§6.1). Without `--yes` it prints each file and pointer it would change and each command it would run; with `--yes` it does them. `--remove` reverses exactly what it added. Nothing happens automatically. `--home DIR` puts `~` at `DIR` (the bench uses it, §9).
- **MCP is delegated** (use, don't rebuild): Itse runs `claude mcp add-json itse '{"type":"stdio","command":"itse","args":["mcp"]}' --scope local` with cwd = the root. Local scope keeps the entry in `~/.claude.json` under that project, in `$HOME`, never in the repo, and Claude Code then runs `itse mcp` only in sessions for that root. Itse never reads or writes `~/.claude.json` itself: Claude Code rewrites it constantly while it runs, and a second writer would clobber it.
- **SKILL.md** is workspace-neutral, at most 60 lines: YAML front matter, the core verbs, the result conventions (verdict line, ids, `show run:N/T1`, exit codes), and `workspace specifics: itse verbs`. Actions, extension skills and `debug` come from `itse verbs` at run time, so one global file is never stale.
- **The hook recipe** is a JSON-pointer merge into `~/.claude/settings.json` that keeps every other key, as Halti's `prepare_home` (run.rs:467-497). `append` is idempotent: an entry deep-equal to Itse's value is not added twice. `--remove` deletes only entries deep-equal to that value.
- Refused: any target outside `$HOME` (or `--home`); any target inside a git work tree or a known workspace root (so never `.claude/` in a repo); a target that isn't valid JSON (`~/.claude/settings.json is not valid JSON; not changed`). A missing `claude` binary for `--mcp` prints the command for the human to run.

**`itse hook post-edit`** reads stdin JSON (≤ 1 MB). The workspace comes from `path_from` (the edited file) and `cwd_from`, never the hook process's cwd, so an edit in `~/.claude` or `/tmp` maps to no workspace and the hook exits at once. Then:
- It **never starts the daemon and never starts a server**. With no daemon socket, or a workspace with no declared server or action, it prints nothing and exits 0.
- Otherwise it calls the daemon in `hook` mode with `problems --new <file>` against live servers only: the edited file is `didOpen`ed or `didChange`d first, so the server's native diagnostics arrive before its flycheck; the settle deadline is 1.5 s.
- The baseline key is the hook's stdin `session_id`, hashed (§3.4).
- If the call settled and there are new errors or warnings, it prints the `wrap` JSON with text capped at 2,000 chars. If it did not settle, it prints nothing and the baseline is not advanced, so the errors are reported by the next settled call instead of being lost or blamed on a later edit. Usage records `settled: false` (§8.2), so the deadline can be tuned from data; the S4 opt-in test measures p50/p95 time to the first post-edit push on rust-analyzer and sets the default from it.
- It always exits 0, and never sets `decision`, `permissionDecision` or `classifierContext`. Only PostToolUse ships in I1.

## 8. Usage log and stats (EMA-212)

### 8.1 Dispatch wrapper

`dispatch::call(caller, verb, args, host) -> Reply` runs, in order: look up the verb (unknown → `unknown_verb`, logged as `"unknown"`, never the caller's string); validate arguments; check trust and human-only rules; start the clock and run; shape, cap, page and store; render; `usage.record(…)`, which fails open. The daemon's `cli`, `hook` and MCP `tools/call` paths all call it. `Local` verbs (`exec`, `env`, `verbs`, `integrate`, `use`, `unuse`) call `dispatch::local`, which uses the same record path with a short-lived writer. `stats` bypasses it.

### 8.2 Records

Path `${XDG_STATE_HOME:-$HOME/.local/state}/itse/usage.jsonl`. Dir 0700 (looser modes tightened, Halti store.rs:122-132). Each record follows Halti audit.rs:215-232: open with `append().create().mode(0o600)`, `File::lock`, one `write_all`, close. Nothing holds the file open, so daemons of several workspaces can share it and a rotation by one of them loses no lines of the others.

```json
{"v":1,"ev":"init","ts":"2026-11-04T18:31:07.412Z","ws":"3f9a1c7e20b4","sess":"9f3a1c7e5d2b4410","client":"claude-code","client_v":"2.1.312","proto":"2025-11-25","tools":4,"list_bytes":2306}
{"v":1,"ev":"call","ts":"2026-11-04T18:32:45.020Z","ws":"3f9a1c7e20b4","sess":"9f3a1c7e5d2b4410","via":"mcp","client":"claude-code","client_v":"2.1.312","verb":"test","ext":"gradle","ms":38211,"outcome":"fail","err":null,"bytes":1834,"full":96211,"trunc":true,"warm":true,"settled":null}
```

| Key | Value |
|---|---|
| `ts` | UTC with milliseconds (hand-written civil-from-days, no time crate) |
| `ws` | first 12 hex of sha256(salt ‖ canonical root), with the 32-byte per-machine `salt` file (§3.2); no path is logged and the id can't be reversed by guessing paths |
| `sess` | 16 hex: the hashed session key of §3.4 (`ITSE_SESSION`, the harness extension's session env, or the hook's `session_id`); for MCP without one, 8 random bytes per connection; for a plain CLI, 8 random bytes per call. Kept apart from the `--new` baseline key |
| `via` | `cli`, `mcp` or `hook` |
| `client`, `client_v` | MCP `clientInfo`; CLI `ITSE_CLIENT=name/version`, else `cli`; clipped to 64 chars, control characters as `?` |
| `verb` | from the closed table, `<server>__<tool>`, or `unknown` |
| `ext` | manifest id of the extension that served the call |
| `outcome` | `ok`, `fail`, `error`, `cancel`, `timeout`; `running` counts as ok, `needs_*` as error |
| `err` | `bad_args`, `unknown_verb`, `unknown_id`, `untrusted`, `needs_human`, `needs_consent`, `not_installed`, `server_start`, `server_crash`, `blocked_by_door`, `timeout`, `io`, `internal`, or null |
| `bytes`, `full`, `trunc` | returned text size, size before capping, whether capped |
| `warm` | the server was warm; null for verbs with no session |
| `settled` | for `problems` and `hook`: whether the settle finished before the deadline; else null |

**Never logged**: argument values, file paths and contents, symbol, test or module names, result text, error messages, env values.

### 8.3 Rules

- Fail open: an open, lock or write error is dropped, with one `itse daemon: usage log: <e>` line in the daemon log per lifetime.
- Rotation: the writer that holds the lock and finds the file over 33,554,432 B renames it to `usage.1.jsonl` (replacing an older one) before appending, all under the same lock, so two writers can't rotate twice in a row. After taking the lock, a writer compares its fd's inode with the path's and reopens if they differ, so no line lands in the renamed file.
- Off: `ITSE_USAGE=off` in the daemon's env, or in the caller's `hello.env` (the CLI's or the `itse mcp` process's), or `"usage": false` in `~/.config/itse/config.json` (re-checked by mtime per call). A repo's team layer has no `usage` key.
- `itse status` always shows the usage-log line (§6.3).
- No upload, ever. `cat` and `rm` are the export and the forget.

### 8.4 `itse stats`

`--since 7d|24h|YYYY-MM-DD` (default 7d), `--by verb|ext|client|ws|day` (default verb), `--json`. It streams `usage.1.jsonl` then `usage.jsonl`, keeps per-group latencies for p50/p95, and is not logged. "Offered, never called" is the current tool list (built in-process from the resolved manifest) minus the tools called in the window.

```
usage 2026-10-28..2026-11-04 (7d): 1,284 calls, 37 sessions, 2 workspaces; log 4.1 MB
verb (ext)      calls  ok   fail err  p50ms  p95ms  ~tok/call  kept out  trunc  warm
test (gradle)     311  61%  36%  3%   8,420 41,000        460    28.1 MB   74%   92%
problems          402  97%   -   3%     140  1,900        210     1.2 MB   12%   98%
show              188  99%   -   1%      12     40        980          -   31%    -
connected, never called: 4 of 37 sessions (claude-code 2.1.312)
offered, never called: outline
errors: server_crash 12 (kotlin-lsp), timeout 5 (gradle)
```
`--json`: `{since, until, calls, sessions, groups: [{key, calls, ok, fail, error, cancel, timeout, p50_ms, p95_ms, bytes, tok_est, kept_bytes, trunc_rate, warm_rate}], idle_sessions, never_called}`. Tokens are estimated as bytes ÷ 4.

## 9. itse-bench (EMA-213)

A separate crate under `bench/`, not a workspace member, with its own `Cargo.lock`, as Halti keeps `box/shim`. It is never linked into the core. Budget ≤ 1K lines; crates: anyhow, serde, serde_json, lexopt, sha2 (all core crates). It installs no agents, runs no containers and downloads nothing.

```
bench/src/main.rs     lexopt: run | report | check-task                                  ~110
bench/src/toml.rs     flat TOML subset: [table], [a.b], key = "str"|int|float|bool|["str",…], # comments;
                      anything else is an error with line:col                              ~70
bench/src/task.rs     load and validate a task dir; task_sha = sha256 of sorted (path, bytes)  ~80
bench/src/adapter.rs  adapters.toml; {placeholder} templating; parsers json-last and none   ~120
bench/src/run.rs      trials, arms, timeout via process group, results.jsonl              ~260
bench/src/report.rs   per-arm pass rate, paired table, exact sign test, medians            ~150
bench/adapters.toml   claude-code; bench/tests/fixtures/adapters.toml holds `fake` (a sh script)
bench/tasks/<id>/     instruction.md, task.toml, environment/setup.sh, tests/test.sh, solution/solve.sh
```

**Task** (Harbor's names; `environment/setup.sh` replaces a Dockerfile; names to be checked against Harbor before freezing):
```toml
version = "1.0"
[metadata]
tags = ["rust", "failing-test"]   # "control" marks control tasks
[agent]
timeout_sec = 600
[verifier]
timeout_sec = 120
[itse_bench]
budget_usd = 1.0
use = ["rust"]                   # the developer layer the `with` arm declares
```

**Adapter:**
```toml
[claude-code]
version_cmd = ["claude", "--version"]
command = ["claude", "-p", "{instruction}", "--output-format", "json", "--strict-mcp-config", "--mcp-config", "{mcp_config}",
           "--max-budget-usd", "{budget_usd}", "--model", "{model}", "--permission-mode", "{permission_mode}"]
parser = "json-last"
[claude-code.fields]   # JSON pointers; a missing pointer is null, never guessed
cost_usd = "/total_cost_usd"
turns = "/num_turns"
fresh_tokens = "/usage/input_tokens"
cache_read_tokens = "/usage/cache_read_input_tokens"
cache_write_tokens = "/usage/cache_creation_input_tokens"
output_tokens = "/usage/output_tokens"
[claude-code.arms.with]
mcp_config = '{"mcpServers":{"itse":{"type":"stdio","command":"itse","args":["mcp"]}}}'
[claude-code.arms.without]
mcp_config = '{"mcpServers":{}}'
```

**Runner** (`itse-bench run --tasks bench/tasks --harness claude-code --reps 3 --seed N --out DIR`):
1. Per task, arms are shuffled per rep with a seeded xorshift (recorded).
2. Each trial dir is `~/.cache/itse-bench/<run>/<trial>/` with `ws/`, `state/` (its `XDG_STATE_HOME`), `cfg/` (its `XDG_CONFIG_HOME` and the clean harness config) and `bin/` (a private dir holding only an `itse` symlink and the shim dir).
3. `setup.sh ws` creates the repo and commits the base.
4. Arm `with`: before the harness starts, the bench runs, with the trial's XDG env, `itse use <ext>` for each `[itse_bench] use` entry, `itse trust` (recorded `non-tty`; the trial record notes `trusted_by: bench`), and `itse integrate <harness> --skill --home cfg/ --yes`. The trial's `bin/` goes first on PATH, and `ITSE_SERVER_IDLE_S=60`. The agent is never prompted to use Itse.
5. Arm `without`: the same PATH without the trial's `bin/`; shared dirs are never filtered, so cargo, rustc and rust-analyzer stay exactly as in `with`. The bench asserts `command -v itse` fails and records `arm_clean`; both arms assert `command -v cargo` succeeds.
6. The harness runs in `ws` in its own process group; on timeout SIGTERM, SIGKILL after 5 s.
7. After the harness exits, the bench reads `state/itse/ws/<id>/daemon.json` and the daemon's VmHWM, then runs `itse daemon stop` with the trial's XDG env (which stops its servers) and asserts that no process with the trial's dirs in its cmdline or environ remains before the next trial.
8. `bash <task>/tests/test.sh` runs from the task dir with `WORKSPACE=ws`; exit 0 is a pass.
9. It records `git diff --shortstat <base>`, summarises the trial's `usage.jsonl` (calls by verb, errors, bytes, list_bytes), and appends one line to `DIR/results.jsonl` with EMA-213's required fields, the optional ones as null when absent, and an `itse` block.

**Report**: pass rate per arm; per-task paired table (both, only with, only without, neither); exact two-sided sign test on discordant pairs, p = min(1, 2·Σ_{k≤min(b,c)} C(n,k)/2ⁿ); medians of wall time, tokens and cost per task and per solved task; Itse calls per trial; the fixed sentence that 10 tasks × 3 reps detect only large effects. Tokens are compared only within one harness.

**Starter tasks** (runnable here): `rust-off-by-one` (a no-dependency crate where `fn window(range)` drops the last element; `test.sh` checks the test files' sha256 is unchanged, runs `cargo test --offline`, then adds a hidden test) and `rust-doc-control` (tag `control`). Kotlin/Gradle tasks come in S8.

**Itse's own baseline, no harness**: the scripted session of §12.2 appends `{startup_ms, p50_ms/p95_ms per verb, vmhwm_kb, list_bytes, commit}` to `target/budget/history.jsonl` on each gate run.

Bench self-tests cover the TOML subset, json-last, the sign test, and a whole trial with the `fake` adapter. They spend nothing.

## 10. Security and trust

**The boundary** is the Halti VM, the door allowlist, and sha256 pins from upstream provenance. The agent is uid 1000 and can edit `~/.config/itse` and run any `itse` command, so Itse's trust records **prevent surprises and record decisions**; they don't stop a hostile agent. Hard enforcement (Touch ID through Halti) is I2.

| Concern | Rule |
|---|---|
| Network | The core opens no sockets except its own Unix socket. Bytes come only through curl via the door. The one exception, pending the owner (O1): DAP TCP connects, loopback only, connect only, checked in code; or, for a literal reading, a delegated python3 stdio↔TCP bridge so the core opens none (§5.5). |
| Pins | Every fetched artifact is sha256-verified before extraction or launch. Mismatch deletes the file. No pin, no install, unless TOFU is accepted by a human. |
| Archives | No absolute symlinks, nothing escaping the tree, `--no-same-owner`, read-only store. |
| Workspace trust | Every execution of project code that Itse starts needs `itse trust`: actions and servers with `executes_project_code` (gradle and cargo actions, kotlin-lsp, rust-analyzer), and every team-layer entry. Before trust, the team layer is read for status only and none of those start. Trust is per repo (§3.1), so worktrees inherit it. **The trust hash covers only what trust is about**: the bytes of the team-layer files in the worktree (empty if none), plus the unrendered `command`, `args`, `options.cwd` and `env` of every used `executes_project_code` entry, with `${pkg:…}` left as written. It excludes the lock (pins are sha256-verified with their own provenance), store paths, built-in version numbers and settings. So `lock --refresh`, an install or an itse upgrade that keeps those entries doesn't revoke trust; a team-layer change or a new project-code entry asks again. The record keeps the last 8 accepted hashes, so switching between two branches doesn't ask each time. |
| Consent | Never defaulted (EULA, region, data sharing). `itse consent kotlin-lsp` shows the extracted `EULA.txt` path and records `{package, eula_hash, region, data_sharing}`. A new EULA hash asks again. |
| Human-only | `trust`, `consent`, `ext add/remove`, `integrate`, TOFU: `needs_human` over MCP; advisory on the CLI with `actor: tty\|non-tty` (whether `consent` should refuse non-tty callers in I1 is owner question O21). |
| Repos | Itse itself writes no file under a workspace root, `.git/` included (no `git status` index refresh, §5.4). The project's declared tools write their normal outputs; each action declares them in `outputs`. Tests: `nothing_writes_inside_the_workspace` snapshots the tree (paths, sizes, mtimes, and `.git/index` inode and mtime) before and after every non-run verb on a fixture with no build actions; `run_verbs_change_only_declared_outputs` runs `test` and `build` on the rust fixture and allows changes only under the action's `outputs`. `integrate` refuses targets inside work trees. |
| Local IPC | Socket 0600 in a 0700 dir checked for owner and mode. The caller's env travels over that socket and is never written or logged. |
| Children | Inherit the box env; argv vectors; own process group; `RLIMIT_CORE=0`; declared rlimits; `oom_score_adj` 800/500. No edits applied (`applyEdit` refused), no `runInTerminal`. |
| Records | Usage log never holds values (§8.2). Run specs hold argv and cwd, never env. |
| Credentials | Itse holds none. The door injects them. Stand-in values in the box are never read or stored. |
| Test seams | Fixture servers are a separate binary (`itse-fixture`, §12.3), never in `itse`. `ITSE_TEST_*` seams exist only under `#[cfg(debug_assertions)]`, and a release build refuses to start when any `ITSE_TEST_*` is set. |
| Install-time code | curl and extraction run nothing. npm `--ignore-scripts` and build.rs warnings are I2. |

## 11. Repo layout, modules and budgets

### 11.1 Layout

```
/work/src/itse/
  Cargo.toml  Cargo.lock  rust-toolchain.toml  rustfmt.toml  budget.toml  README.md  .gitignore
  src/main.rs                      lexopt CLI, cmd_* functions, argv[0] shims, `// --- section ---` banners
  src/<module>.rs + src/<module>/*.rs   never mod.rs; big tests in src/<module>/tests.rs
  ext/<id>/itse-ext.json           built-ins: rust, jdk, kotlin, gradle, node, bun, claude-code
  docs/design.md                   this file
  docs/verbs.md                    generated by `itse verbs --md`, golden-tested
  docs/manifest.md                 the manifest and extension format; every json block parse-tested
  scripts/check.sh  scripts/budget.sh  scripts/budget.py
  tests/e2e.rs (+ tests/e2e/*.rs)  drives env!("CARGO_BIN_EXE_itse") and env!("CARGO_BIN_EXE_itse-fixture")
  tests/fixture/main.rs + {lsp,dap,mcp,tool}.rs   the `itse-fixture` binary: fake servers and delegates
  tests/fixtures/rust-ws/          tiny cargo crate with its own empty [workspace]; copied to a tempdir
  tests/fixtures/ext/*.json        test extensions pointing at `itse-fixture …`
  bench/                           Cargo.toml Cargo.lock rustfmt.toml src/ adapters.toml tasks/ tests/fixtures/
```
- `.gitignore`: `/target`, `/bench/target`. `docs/` is committed (EMA-206 needs the docs in the repo).
- No `mise.toml` and no `.github/`. Plain scripts are the interface.
- `rustfmt.toml` is Halti's, verbatim (`max_width = 180`, `use_small_heuristics = "Max"`, with its comment); `bench/` gets the same file.

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.92.0"
components = ["rustfmt", "clippy", "rust-analyzer"]
profile = "minimal"
```

```toml
# Cargo.toml
[package]
name = "itse"
version = "0.1.0"
edition = "2024"
rust-version = "1.92"
publish = false
default-run = "itse"
autotests = false                    # tests/fixture/main.rs is a binary, not a test (found in S0)
exclude = ["bench/", "tests/fixtures/"]

[[bin]]
name = "itse"
path = "src/main.rs"

[[bin]]
name = "itse-fixture"               # test-only; never installed (`cargo install --path . --bin itse`)
path = "tests/fixture/main.rs"
test = false

[[test]]
name = "e2e"
path = "tests/e2e.rs"

[dependencies]
anyhow = "1.0.104"
lexopt = "0.3.2"
regex-lite = "0.1.9"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
sha2 = "0.11.1"

[lints.rust]
unsafe_op_in_unsafe_fn = "deny"
[lints.clippy]
undocumented_unsafe_blocks = "deny"   # enforces Halti's // SAFETY: notes

[profile.release]
lto = true
strip = true
codegen-units = 1
```
- No dev-dependencies. `itse-fixture` uses only serde_json and std, not Itse's modules, so the codec is never tested against itself.
- `panic` stays `unwind`: a panic in one connection must not take down warm servers. `abort` saved nothing on the probe.
- `opt-level` stays 3: `z` saved 6% (1.06 vs 1.13 MB). Re-measure at S3.
- Hand FFI, not libc (as Halti). Rejected crates: clap (+14), toml, ureq/rustls (sockets), tokio, rmcp (74 crates), lsp-types, dap, quick-xml, regex.

**Crate allowlist** in `budget.toml` (25 names, root included): itse, anyhow, block-buffer, cfg-if, const-oid, cpufeatures, crypto-common, digest, hybrid-array, itoa, lexopt, libc, memchr, proc-macro2, quote, regex-lite, serde, serde_core, serde_derive, serde_json, sha2, syn, typenum, unicode-ident, zmij. libc arrives through cpufeatures on aarch64 only, so x86_64 resolves 24 of them; Itse doesn't use libc directly. The gate's rule is per target (§12.1).

### 11.2 Module map

A module is `src/<m>.rs` plus `src/<m>/**`. Lines are code lines after `cargo fmt` (§12.1); closing braces and `#[derive]`/`#[serde]` lines count. A module without a cap fails the gate.

**The caps were provisional until S0 re-baselined them; this is the re-baselined table (2026-11-05).** Dense sketches don't survive rustfmt: with Halti's `rustfmt.toml`, struct and enum definitions still put every field on its own line, so `Limits` goes from 1 line to 7 and the §6.2 types from about 10 to about 60. S0 therefore wrote every serde model and public type for real (manifest, extension, lock, trust, consents, found, procs.json, next.json, run.json, baselines, usage records, `Launch`, `Outcome` and friends), formatted them, counted them, and re-set the caps. Rules that keep models small: records that are only read (harness integration data, `daemon.json` read by the CLI, procs.json on restart) use `serde_json::Value` with pointer access instead of derive structs; small on-disk records are grouped (`run.json` replaces spec/pid/exit files); derive structs are kept where `deny_unknown_fields` does the validation for free (extensions and layers). If the re-baselined total passes 5,000, the cut list below applies in order, and the owner gets the numbers before S1 starts.

**How the caps were re-set.** For `main`, `sys` and `workspace`, which S0 finishes, the cap is the measured code (`main` keeps 150 for the CLI that S1 to S6 add). For each module that holds S0 models, the cap grows by the models' measured lines minus their dense form, the one line per type (about one per 8 fields for wide types) that the sub-design estimates counted: `results` 76 measured vs 12 sketched (+64), `host` 60 vs 10 (+50), `manifest` 261 vs 44 (+217; 25 types, about 150 fields, the VS Code shapes `OneOrMany`, `Group` and `MatcherRef` included), `lock` 52 vs 9 (+43), `actions` 36 vs 5 (+31), `parse` 8 vs 1 (+7), `usage` 39 vs 4 (+35). `daemon.json` is not a derive struct (it is read through `Value` and written with `json!` in S1, inside `daemon`'s cap). The S0 measurements include each module's `use` lines and one `#![allow(dead_code)]` that S1 to S3 remove. `sys` measured 168 against 130 (+40 with 2 of slack) and `workspace` 93 against 80 (+13, after the S0 review: a missing or relative `HOME` is refused, the `/tmp` fallback is length-checked).

**The result is over 5,000, so the whole cut list below is applied**, in order: the relay's reconnect and replay (`daemon` −40), `debug eval`/`step` (`dap` −40), `gc` only `--keep-locked` (`verbs` −20), the outside-server note (`verbs` −12), one `lock --refresh` adapter (`pins` −50). Even after all of it the caps sum to **5,188**, 188 over the gate with no reserve left (the reserve held 150). **This needs the owner before S1** (§14 O23). The gate itself checks real lines, which are 874 after S0, so it stays green until real code passes 5,000.

| Module | Core part | Owns | Cap (I1 plan) | S0 lines | Cap (re-baselined) | Slice |
|---|---|---|---|---|---|---|
| main | exposure | lexopt into `Cmd`; argv[0] shim dispatch; `__run`, `__daemonize`; printing; exit codes | 150 | 81 | 150 | S0+ |
| sys | shared | hand FFI (`kill`, `setsid`, `setrlimit`, `getuid`, `signal`, `mallopt`); `proc_stat`, `kill_group`; 0700/0600 and tightening; atomic write; `/dev/urandom` ids and salt; RFC 3339 UTC; refusing `ITSE_TEST_*` in release; the poison-tolerant `lock` | 130 | 168 | 170 | S0 |
| workspace | host | root, repo key from `.git` files, ids, XDG paths (absolute or refused), socket path and `/tmp` fallback checks | 80 | 93 | 93 | S0 |
| rpc | host | framings, dialects, `Peer`, `Cancel`, oversize drain | 200 | – | 200 | S1 |
| daemon | host | lifecycle, lock wait, `serve_conn`, idle, signals and self-wake, socket re-bind, version and exe check; `daemon/client.rs`: connect, `__daemonize`, hello timeout, Ctrl-C loop, `itse mcp` relay (no reconnect and replay: cut) | 290 | – | 250 | S1 |
| results | verbs | `Outcome`, rendering (`next` per caller), caps, pages, `TEXT`, results store and retention, `show` with grep and tail | 230 | 76 | 294 | S1 |
| usage | exposure | the EMA-212 writer (per-append open, rotation under lock, salted ids) and `stats` | 230 | 39 | 265 | S1 |
| verbs | verbs, exposure | `VERBS`, dispatch wrapper, verb bodies (status with suggestions, use/unuse, ext, trust, consent, install, lock, gc `--keep-locked`, run/build/test, problems and baselines, nav, debug, stop, cancel), index, `integrate`, `hook` | 600 | – | 568 | S1–S6 |
| mcp | exposure | `serve_io`, negotiation, tool list and list cap, `instructions`, `tools/call` with `args`; `mcp/client.rs` hosted servers, allowlist, first listing | 300 | – | 300 | S1, S6 |
| manifest | resolver | serde model, built-ins, layers, detection, developer-layer lookup and writes, validation, templates, ignored VS Code keys | 350 | 261 | 567 | S2 |
| resolve | resolver | states, targets, `requires`, project pin readers, typed system checks | 240 | – | 240 | S2 |
| lock | resolver | lock, trust and its hash, consents, found | 200 | 52 | 243 | S2 |
| store | resolver | `Delegate` (curl, tar, gzip, python3, keytool), door classes, verify, extract, walk, `install`, `gc --keep-locked` | 320 | – | 320 | S2 |
| pins | resolver | one of `github-release` and `openvsx-bundle` (the other pin entered by hand: cut), expiry, lock diff | 120 | – | 70 | S2 |
| env | resolver | env composition, shims, `exec`, `env --sh`, truststore | 140 | – | 140 | S2 |
| actions | host | `run.json`, task expansion, `dependsOn`, `__run` runner, wait and patterns, cancel and timeout, re-adoption | 200 | 36 | 231 | S3 |
| parse | verbs | matcher engine, streamed junit-xml, trace trimming, counts, fingerprints, baselines, merge and order | 250 | 8 | 257 | S3 |
| host | host | `Launch`, `Limits`, registry, slots (phase and live), handles, `procs.json`, `next.json`, spawn and limits, crash state, restart cleanup | 210 | 60 | 260 | S3, S4 |
| lsp | host | LSP client, disk sync, settle, cold flag, queries, name matching | 330 | – | 330 | S4 |
| dap | host | DAP client, program/tcp/lsp_command modes, default stop rule, snapshot, continue and stop (no eval or step: cut) | 280 | – | 240 | S5 |
| **Total of caps** | | | **4,850** | **874** | **5,188** | |
| Reserve | | modern MCP era (~40), slack | 150 | | 0 | |
| **Gate** | | | **5,000** | | **5,000** | |

Ways back under 5,000 for the owner to pick from (none applied): a `model!` macro that writes the derive and serde lines once per struct (about −50, less plain code); `serde_json::Value` instead of derive structs for `Action` and `Matcher` (about −90, but unknown keys in pasted tasks would then pass silently); dropping the VS Code string forms of `group` and `dependsOn` (about −20; pasted tasks.json entries then need editing); moving multi-line matchers to I2 (needs another way for `ext/rust` to find panics); raising the gate to 5,200 lines.

**Inside `verbs` (600 planned, 568 after the S0 cuts)**: table and dispatch 100; workspace verbs (status, use/unuse, ext, trust, consent, install, lock, gc) 125, now 93 without the outside-server note and with `gc --keep-locked` only; run/build/test 80; problems and baselines 75; nav 70; debug 60; index 20; hook 30; integrate 40.

**How this compares with the sub-design estimates**: the ecosystem estimated about 2,260 for the resolver and parsers; here it is 1,370 for the resolver plus 250 for parsers, because cargo-json is gone, door classes sit in `store`, and `doctor` folded into `status`. The session host estimated 1,650 including runs and the MCP client; here it is 1,590 plus the MCP client inside `mcp`, after cutting pull diagnostics. For scale, Halti is 9,145 code lines; Itse's 5K holds an LSP, DAP and MCP client, an MCP server, an installer, parsers and stats, which is why the caps are tight and re-measured in S0.

**Already cut** to pay for the review fixes: scanning a TCP adapter's stdout for its port (−20); the non-git mtime walk (−20); `install --offline` (−10); `ext call` on the CLI (−10); MCP Tasks in I1 (−80 from the reserve); fixture code and the fixture-guard gate (moved to `tests/`).

**Cut list if a module goes red**, in order (all of it applied in S0, see above): the relay's reconnect and replay (−40, the relay then exits on daemon EOF and the harness restarts it); `debug eval`/`step` down to snapshot, continue and stop (−40); `gc` keeps only `--keep-locked` (−20); the outside-server `/proc` note (−12); one `lock --refresh` adapter (−50, the other pin is then entered by hand). Multi-line matchers can move to I2 only if the `rust` built-in gets another way to find panics.

### 11.3 Other budgets

| Budget | Limit | Expected |
|---|---|---|
| Binary | ≤ 5,242,880 B, static | ~1.1–1.5 MB (glibc `+crt-static`; 1.06 MB measured for the crate set; 995,760 B for the S0 binary, aarch64) |
| Crates | ≤ 60; each target a subset of the allowlist, their union equal to it | 25 (aarch64), 24 (x86_64) |
| Daemon RSS | VmHWM ≤ 20,480 kB in the scripted session, children excluded; also recorded (not gated) in the opt-in rust-analyzer run against Halti's 9K-line crate | probe daemon peaked at 1.7 MB after 50 connections; 6–8 threads in a typical session. Peaks are bounded by design: 4 MiB frames with drain, 2,000 compact diagnostics per server, streamed JUnit, a 50K-name file index, `M_ARENA_MAX=2` |
| First-party extensions | ≤ 1,000 lines each (`ext/<id>/`, `bench/src`) | `bench` ~790 |
| Usage log cost | mean < 1 ms per append | – |

## 12. Gates and tests

### 12.1 Gates (local, fail closed)

`scripts/check.sh` runs with `set -euo pipefail`. It first checks with `command -v` that cargo, rustfmt, cargo-clippy and python3 exist. Then:
1. `cargo fmt --check`, and the same in `bench/`.
2. `cargo clippy --locked --all-targets -- -D warnings`, and the same in `bench/`.
3. `cargo test --locked`, and the same in `bench/`.
4. `python3 -I scripts/budget.py --self-test`.
5. `scripts/budget.sh`, which runs `python3 -I scripts/budget.py budget.toml` (`tomllib` is in python 3.11).

Each gate prints `PASS <gate> <value>/<limit> (<seconds> s)` or `FAIL <gate>: <reason>`, and check.sh prints its total time. A missing tool, an exception or unparsable output is a FAIL. `check.sh --opt-in` adds the opt-in tests (§12.3). It runs before every commit; that is how "a PR that breaks a budget fails" is met without CI (decision 3).

| Gate | Method | Limit |
|---|---|---|
| toolchain | `rustc -V` starts with `rustc 1.92.0` | equals `budget.toml toolchain` |
| size | `RUSTFLAGS="-C target-feature=+crt-static" cargo build --locked --release --target <host triple> --target-dir target/budget`; `stat`; ELF program headers read in Python, a `PT_INTERP` fails "static" | ≤ 5,242,880 B, static |
| crates | `cargo tree -e normal,build --prefix none --target <t>` with ` (*)` and ` (proc-macro)` stripped, `sort -u`, for aarch64-unknown-linux-gnu and x86_64-unknown-linux-gnu. Each target's set must be a subset of `[crates].allow`, and the union over both targets must equal it (today 25 on aarch64, 24 on x86_64) | union ≤ 60, no unlisted name, no stale allowlist entry |
| lines | after `cargo fmt`, non-blank lines under `src/` that aren't comment-only, skipping `tests.rs` and `*_tests.rs` files, stopping at the first column-0 `#[cfg(test)]` in a file; `tests/` (with the `itse-fixture` binary) is test code and not counted; raw lines printed for information; per-module caps, total, unknown modules fail | total ≤ 5,000 |
| ext-lines | each `ext/<id>/` and `bench/src` | ≤ 1,000 each |
| rss | the scripted session (§12.2) | VmHWM ≤ 20,480 kB |
| usage-cost | `usage::tests::append_costs_under_a_millisecond` (2,000 appends) in the normal debug test build: if it holds in debug, it holds in release, and check.sh doesn't build the test suite a second time with LTO | mean < 1,000 µs |

Before S1 the rss gate is `enabled = false`, and it fails if `src/daemon.rs` exists while disabled, so it can't stay off silently.

`--self-test` plants an over-cap module, an unknown module, an unlisted crate and a stale allowlist entry (canned two-target `cargo tree` text, where x86_64 lacks libc and still passes), an oversized and a dynamic ELF stub, an oversized ext dir, a fake `/proc` status, and a run with an empty PATH. Each asserts that exactly the named gate fails.

### 12.2 Scripted session

`budget.py session` uses a tempdir under /tmp with a private `HOME` and `XDG_*`. The release-static `itse` is the daemon; `target/debug/itse-fixture` serves as the fixture children, declared in a generated developer layer and trusted. It runs 200 calls (status, a fixture action through `run` and `test`, problems, show, a page, find and refs against the fixture LSP) and `itse mcp` with initialize, tools/list and 20 tools/call. Then it reads the daemon's VmHWM from `/proc/<pid>/status` and prints one JSON line, appended to `target/budget/history.jsonl` (§9).

### 12.3 Test strategy

- **Unit tests** sit inline while a module is small and move to `src/<m>/tests.rs` past about 150 lines (Halti). Names are behaviour sentences. Pure builders (curl, tar and keytool argv, env composition, rlimits) are tested by exact argv or env equality.
- **Seams** are debug-only: `ITSE_TEST_NOW` (clock), `ITSE_TEST_DELEGATE_DIR` (delegates resolved there first), `ITSE_TEST_PROTO`. `ITSE_DAEMON_IDLE_S`, `ITSE_SERVER_IDLE_S` and `ITSE_USAGE` are real config. Unit tests use in-process fakes behind traits (`Delegate`, a `World` for the clock and `/proc`); anything needing a fake server is an integration test under `tests/`.
- **Re-exec scenarios** re-run the test binary with `--exact NAME` and a private env (Halti mcp/tests.rs:29-52).
- **Fixture servers**: `itse-fixture lsp|dap|mcp|tool`, a second binary target built from `tests/fixture/` (C12). It uses serde_json directly, not `rpc.rs`, so the codec isn't tested against itself. `ITSE_FIXTURE_SCRIPT=<json>` gives the scenario; `ITSE_FIXTURE_MODE=crash-after-init|garbage-frame|oversize-frame|slow|exit:<n>|ignore-sigint|java-order|caught-only` drives error paths.
  - `lsp`: capabilities; a `workspace/configuration` request; progress begin/end; diagnostics for files containing `FIXTURE_ERROR`; symbols, definition and references from the script; `start_debug_server` returning a port on 127.0.0.1.
  - `dap`: stdio or `--tcp`; initialize, launch/attach, breakpoints, configurationDone, `stopped{exception}`, stackTrace with mixed frames, scopes, variables, evaluate, continue, terminated; `caught-only` offers only a plain `caught` filter and first stops in a framework frame.
  - `mcp`: tools `echo` and `slow`.
  - `tool curl|tar|keytool`: curl-argv-compatible, serves URLs from a mapping dir, emits the `-w` output, simulates door classes (`blocked.test` → exit 56 with CONNECT 403, `badtls.test` → 502).
- **e2e** (`tests/e2e.rs`) drives `CARGO_BIN_EXE_itse` with a private `HOME` and `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` under `/tmp/itse-e2e-<pid>-<n>` (short for sun_path), `XDG_RUNTIME_DIR` removed, `ITSE_DAEMON_IDLE_S=2` and `ITSE_SERVER_IDLE_S=2`, and carries Halti's `Mcp` client (e2e.rs:117-170). Each test's `Env` guard runs `itse daemon stop` on drop, then `kill_group` on the pid in `daemon.json`, so a failed test leaves no daemon behind.
- **Default runs** need only cargo. The cargo actions test copies `tests/fixtures/rust-ws` (a failing test, and a compile-error variant toggled by file swap) into a tempdir with its own `CARGO_TARGET_DIR`. Without cargo: `skipped: needs cargo`.
- **Opt-in** (`#[ignore = "needs …: ITSE_E2E_X=1"]`, run by `check.sh --opt-in`):
  - `ITSE_E2E_RA=1`: real rust-analyzer (`path`). `problems` shows the type error within 30 s; an edit to an unopened module shows the new error; find/refs/outline/info resolve, including `data::Region.save`, a free function in a module, and `--kind method`; the first query says `index cold`; untrusted, a `build.rs` that touches a file under the test's tempdir never runs; p50/p95 time from an edit to the first post-edit push (sets the hook deadline); VmHWM against a copy of Halti's crate, appended to `history.jsonl`.
  - `ITSE_E2E_FETCH=1`: the pinned rust-analyzer `.gz` (14.7 MB) through the door: verify, store, shim, offline reinstall, a sha256-mismatch plant.
  - `ITSE_E2E_CLAUDE=1`: zero-spend smoke; `itse integrate claude-code --mcp --yes` in a throwaway Claude Code config, `claude mcp list`; the `init` record shows the negotiated version; whether Claude Code re-lists on `list_changed`.
  - `ITSE_E2E_CLAUDE_PROBE=1` (needs owner approval, small spend): does a fresh session, unprompted, find the Itse tools through tool search for "run the failing test" and "where is this symbol used"?
  - `ITSE_E2E_CLAUDE_PAID=1`: one `claude -p --max-budget-usd 0.10 --strict-mcp-config` call. Needs owner approval.
  - `ITSE_E2E_JVM=1`: the JVM box only (P-JVM and S8).
- **Doc tests**: `docs/verbs.md` equals `itse verbs --md`; every fenced `json` block in `docs/manifest.md` parses against the model; every `ext/*/itse-ext.json` validates.

### 12.4 Key tests by area

| Area | Tests |
|---|---|
| rpc | round trips in both framings; a reader yielding 1 byte at a time; mixed-case `content-length` with `Content-Type`; `an_oversized_body_is_drained_and_the_peer_stays_usable`; truncated bodies; non-JSON lines skipped; out-of-order responses; a timeout sends `$/cancelRequest`; a server request answered while a client request waits; EOF fails pending requests; `a_panicking_handler_marks_the_peer_crashed` |
| workspace | `the_workspace_of_a_worktree_is_the_worktree`; `a_worktree_and_its_main_checkout_share_a_repo_key` (checked against `git rev-parse --git-common-dir`); a submodule's repo key; nested roots; stable ids; `the_socket_path_ignores_xdg_runtime_dir`; `a_long_state_path_puts_the_socket_in_tmp`; a non-private `/tmp` dir is refused |
| daemon | `the_first_command_starts_the_daemon_and_the_second_reuses_it`; `two_racing_commands_start_one_daemon` (5 in parallel); `a_command_during_shutdown_gets_a_new_daemon`; `the_daemon_exits_when_idle`; `sigterm_stops_an_idle_daemon_within_a_second`; `a_removed_socket_is_rebound`; `a_wedged_daemon_fails_hello_in_5_s`; a stale socket after SIGKILL is replaced; `hello required`; proto mismatch restarts; `replacing_the_binary_under_a_live_daemon_still_runs_tests` (then the next idle call restarts on the new exe); `the_daemon_is_not_a_child_of_its_starter`; `ctrl_c_in_the_cli_cancels_a_slow_query` |
| host | pgid == pid; `RLIMIT_NOFILE` in `/proc/<pid>/limits`; `oom_score_adj` 800; `JAVA_TOOL_OPTIONS` appended, parent vars inherited, `LANG` only when unset; `servers_get_no_itse_or_claude_vars`; stderr in the server log; exit 7 → `server_expired`; idle stop; `a_crashed_server_restarts_on_next_use`; `status_answers_while_a_server_is_starting`; `a_zombie_counts_as_dead` |
| restart | `killing_the_daemon_keeps_results_showable`: SIGKILL during a run, then `run.json` gets its exit, `show run:1` works, the LSP handle is `gone` then restarts cold, a PID with the wrong start time is ignored, the relay answers the in-flight `tools/call` with an `isError` result and replays `initialize` |
| resolver | `a_fresh_repo_shows_suggestions_with_itse_use_as_next`; `use_and_unuse_write_only_the_developer_layer`; `a_new_worktree_inherits_the_developer_layer_and_trust`; `status_reports_a_missing_server_with_a_remedy`; `a_team_layer_is_read_but_never_written`; `nothing_writes_inside_the_workspace` (with `.git/index` inode and mtime); `run_verbs_change_only_declared_outputs`; `refresh_does_not_revoke_trust`; `a_team_layer_change_asks_for_trust_again`; `vs_code_ui_keys_are_ignored_and_unknown_keys_refused`; `a_sha256_mismatch_deletes_the_download`; `a_complete_part_file_is_verified_without_curl`; `curl_exit_33_retries_without_resume`; `blocked_by_door_names_the_host_and_actor`; `an_absolute_symlink_in_an_archive_is_refused`; `gc_removes_read_only_packages`; `a_shim_execs_the_store_binary_or_the_next_on_path`; `shims_point_at_the_cli_path`; `refresh_writes_a_lock_diff_and_installs_nothing`; `expiring_shows_seven_days_ahead`; `untrusted_workspace_runs_no_project_code` (no action, no kotlin or rust-analyzer server starts) |
| actions and parsers | `cargo_test_reports_the_failing_test_at_its_line_with_source`; `cargo_test_runs_every_test_binary_after_a_failure`; `a_panic_line_without_pid_still_matches`; `the_backtrace_note_is_not_a_detail_line`; `a_compile_error_is_E1_with_the_source_line`; `two_default_test_actions_are_an_error_with_candidates`; `gradle_report_with_cdata_entities_and_skips_parses`; `reports_older_than_the_run_are_marked_stale`; `a_malformed_report_does_not_hide_the_others`; `framework_frames_collapse_to_a_count`; `lookbehind_matchers_are_refused_at_load`; `a_timed_out_action_kills_its_process_group`; SIGINT → SIGTERM → SIGKILL; the ready-pattern capture |
| results | `forty_one_items_page_without_splitting_an_item`; `json_pages_hold_the_same_items_as_text`; `templates_hold_no_imperatives` (`instructions` included); `paths_are_repo_relative_with_the_source_line`; `the_full_log_is_one_show_away`; `printed_pointers_are_fully_qualified`; `a_bare_item_id_resolves_from_the_cli`; `next_renders_as_a_tool_call_over_mcp`; `show_grep_and_tail` |
| problems | `problems_new_gives_a_delta_across_two_cli_calls`; `two_hook_sessions_keep_separate_baselines`; `an_unsettled_call_does_not_advance_the_baseline` |
| lsp | the exact capabilities JSON; `server_requests_are_answered`; progress → cold/busy/warm (`the_first_query_on_a_cold_index_says_so`); an on-disk edit → `didChange` + `didSave`, a delete → `didClose`; utf-16 on a non-ASCII line; settle times out to `settled: false`; `find_by_name_returns_candidates_on_ambiguity`; `a_qualifier_matches_the_last_container_segment_or_the_path`; `problems_is_empty_after_a_no_op_edit`; `diagnostics_past_2000_are_counted_not_kept` |
| dap | an adapter answering `launch` only after `configurationDone`, and one answering at once; snapshot caps against 100 variables, deep nesting and long strings; `hide_vars`; adapter EOF → `gone`; `debug_test_stops_at_the_exception_in_one_call`; `debug_test_without_at_breaks_at_the_failing_tests_frame`; `a_caught_only_adapter_skips_framework_stops`; `lsp_command_mode_connects_to_the_returned_port`; a non-loopback address is refused |
| mcp | `initialize_echoes_supported_and_answers_newest_otherwise` (2025-06-18, 2025-11-25, 2025-03-26, junk); `unknown_method_is_32601`; `a_tool_failure_is_isError_not_an_rpc_error`; `tool_list_with_instructions_stays_under_3600_bytes`; `debug_tool_absent_unless_opted_in`; `every_cli_flag_is_reachable_over_mcp`; `ext_add_over_mcp_needs_a_human`; `integrate_is_not_reachable_over_mcp`; `untrusted_test_returns_needs_human_not_an_error`; `cancel_kills_the_run_process_group`; client: version check, pagination, namespacing and the 64-char cap, `exposed_tools_past_the_cap_are_dropped_and_named`, the allowlist, `a_first_listing_waits_10_s_then_sends_list_changed`, a crash mid-call |
| exposure | `cli_and_mcp_print_identical_text`; `hook_never_starts_the_daemon`; `hook_never_starts_a_server`; `hook_takes_the_workspace_from_the_edited_file`; `hook_prints_nothing_when_no_new_errors`; `integrate_mcp_runs_claude_mcp_add_json_at_local_scope` (fake `claude` on PATH); `the_hook_recipe_is_added_once_and_removed_exactly`; `integrate_refuses_a_target_inside_a_work_tree`; `integrate_leaves_invalid_json_untouched`; `the_skill_is_the_same_in_every_workspace` |
| usage | `usage_lines_hold_no_values` (sends `sk-ant-zqx-0123`, `/work/secret/Thing.kt`, `OrderRepository`, `saves_order`, a file body, an error message, the unknown verb `tset-zqx`, a client name with ESC and a `CLAUDE_CODE_SESSION_ID` through CLI, MCP and hook; asserts none appear, every key is allowlisted, every string is in its enum or matches its pattern; mirrors Halti audit.rs:396-408); `the_ws_id_is_salted`; `itse_usage_off_in_the_relay_env_writes_nothing`; `config_off_writes_nothing`; `a_33_mb_log_rotates_once_under_two_writers`; `an_unwritable_state_dir_still_answers`; `modes_are_0700_and_0600`; `stats_calls_are_not_counted`; `stats_groups_by_verb_and_client` |

## 13. I1 plan

Each slice is one committable unit with `check.sh` green. Nothing is committed or pushed until the owner asks. "Cap total" is the sum of the caps of the modules started by the end of the slice (§11.2), before S0's re-baseline; `verbs` and `mcp` start in S1 and grow into their caps until S6.

| Slice | Contents | Linear | Acceptance (beyond the §12.4 tests of its modules) | Cap total | Date |
|---|---|---|---|---|---|
| **S0** scaffold and gates | Cargo files (two bins), toolchain, rustfmt, `budget.toml`, scripts with `--self-test`, README scope paragraph, `main`/`sys`/`workspace`, `itse --version`, `help`, a local `status` that prints the workspace and repo lines; **every serde model and public type written for real, formatted and counted, caps re-baselined** | EMA-189, EMA-177, EMA-174 | `check.sh` passes; every plant fires its gate; no python3 on PATH fails the run; the crates gate passes on both targets (25 aarch64, 24 x86_64, union = allowlist); static binary < 1.2 MB; the re-baselined cap table is written into this file | 360 | 11-05 |
| **S1** kernel | rpc, daemon and client (`__daemonize`, lock wait, self-wake, socket re-bind, exe check), relay, `VERBS` and dispatch with MCP `args`, results and `show` (grep, tail), usage and `stats`, `mcp::serve_io` with the three tools and `instructions`, minimal `status`, `verbs` index, `itse-fixture` skeleton; rss gate on | EMA-188, EMA-212, EMA-206 (verbs doc) | daemon, MCP and usage tests; the opt-in `ITSE_E2E_CLAUDE` smoke records the protocol version and `list_changed` behaviour (week-1 probe) | 2,210 | 11-10 |
| **P-JVM** probe (half a day, in a JVM box) | kotlin-lsp 263.x from the image, headless over stdio with a Python LSP script: consent flags, exit codes 7 and 11, `start_debug_server` and its result, pushes for unopened files, file watching, RSS; `gradlew test --debug-jvm` and the `ready` line; door hosts per package | EMA-214, EMA-207 | written facts in §14 O15; the S5 transport and adapter chosen (below) | – | 11-12..11-18, needs the box |
| **S2** ecosystem | manifest and layers, detection, `use`/`unuse`, repo-keyed developer layer, lock and trust, built-ins (`rust` and `node` as path; `jdk`, `kotlin`, `gradle`, `bun` as data), resolve and status, consent (advisory), store, `pins` against recorded API JSON, env, shims, exec, truststore via fake keytool | EMA-195, EMA-206 (manifest doc) | resolver tests; opt-in `ITSE_E2E_FETCH` | 3,580 | 11-17 |
| **S3** actions and results | host registry (phase and live), actions and `__run` via `/proc/self/exe`, parse, `run`/`build`/`test [filter]`/`problems [--new]` with baselines, `stop`, `cancel`, `ext/rust` actions and matchers | EMA-185, EMA-186 | actions and parser tests on the copied rust-ws fixture | 4,240 | 11-21 |
| **S7** bench | runner, report, two Rust tasks, fake adapter | EMA-213 | a fake-adapter trial end to end, including `use`, trust, the private `bin/` and the daemon stop; the `without` arm recorded with Claude Code once the owner settles credentials and spend (before dogfood) | – (bench) | 11-25 |
| **S4** LSP | `lsp`, rest of `host`, nav verbs, LSP diagnostics in `problems`, cold flag, idle stop | EMA-200, EMA-186 | LSP tests on the fixture; opt-in real rust-analyzer, which also sets the hook deadline from measured p50/p95 | 4,570 | 11-28 |
| **S5** DAP | `dap` on the fixture with the transport P-JVM chose, the default stop rule, one-call `debug test`, step/eval/stop | EMA-207 (core side) | DAP tests; attach success counted in usage | 4,850 | 12-04 |
| **S6** hosting and Claude Code | `mcp/client.rs` and exposure of `expose: true` servers with the list cap, `ext/claude-code`, `integrate` (delegated MCP, idempotent recipe), `hook`; dogfood starts | EMA-188, EMA-187, EMA-191 | exposure tests; skill ≤ 60 lines and workspace-neutral; hook output ≤ 2,000 chars; `itse` built with `cargo install --path . --bin itse` into the owner's box and the hook recipe registered there by the owner | 4,850 | 12-08 |
| **S8** JVM box acceptance | jdk and kotlin-lsp from the store; `gradle:test` through JUnit XML; kotlinc matchers in `ext/kotlin`; the DAP adapter of P-JVM against the monorepo; Kotlin bench tasks; the I1 done-when demo with Claude Code | EMA-182, EMA-207, EMA-214 | "the four parts work on the Kotlin project in a Halti box, with Claude Code", through both the CLI with the skill and MCP | – | 12-08..12-17 |
| Buffer | | | | | 12-17..12-19 |

**Implemented first, in this session**: S0, S1, S2, S3, in that order. The stretch is S4 against the fixture and real rust-analyzer, and the S7 skeleton with the fake adapter, if S3's line count leaves room.

**JVM risk, moved forward.** Every JVM unknown used to sit in S8 with two days of buffer, and kotlin-lsp builds expire about every 30 days (the current one about 11-16..26, EMA-214). So:
- **P-JVM runs by 11-18**, before S5 starts. It needs only a JVM box with the monorepo and the image's existing kotlin-lsp; Itse is not needed for it.
- **S5's transport follows P-JVM**: kotlin-lsp's `start_debug_server` if it exists and returns a port; else the fallback adapter, fwcd kotlin-debug-adapter in attach mode against `--debug-jvm`, or java-debug inside jdtls (both pinned through `github-release`). Either is `tcp` or `lsp_command`, and needs O1.
- **If neither works by 12-04**, the I1 DAP proof is the fixture over stdio plus the fixture over TCP, and the JVM adapter moves to I2 (due 2027-01-30, inside the 3-month window) with the owner's sign-off. No JDI helper is written: it would be non-Rust and can't be built in this box.
- **Delivery to the JVM box**: a static aarch64 `itse` built here (the agent-jvm image has no Rust toolchain), copied in through a path the owner names (a release branch clone under `/work`, or `/downloads`). Nothing is built in the JVM box.
- **Door hosts** the JVM box must allow, per package: kotlin-lsp `open-vsx.org`, `download.jetbrains.com`; Temurin and rust-analyzer `api.github.com`, `github.com`, `release-assets.githubusercontent.com`; fallback adapters the same GitHub hosts. `itse status --hosts` checks them before S8.

**Mapping to the consolidated I1 checklist**: repo bootstrap (S0); gates (S0, rss from S1); manifest and resolver with detected, team and developer layers (S2); session host (S1, S3–S6); verbs and results (S1, S3–S5); exposure with one hosted MCP server and the generated index (S1, S6); one extension per contract: Gradle actions (`ext/gradle`, S8 proof; `ext/rust` here), one language server (rust-analyzer here, kotlin-lsp in S8), one debug adapter (fixture here, JVM in S8 per P-JVM), one hosted MCP server (fixture here, real one O6), Claude Code integration (S6); dogfood started (S6); not in I1: test selection and guards (EMA-192), Henki hooks (EMA-211), rename and refactoring (EMA-210), releases (EMA-197), supervision (EMA-193), MCP Tasks.

## 14. Open points

Each needs the owner, or a probe that the plan already schedules. Points with a date block the plan if they slip.

| # | Point | Proposal | Needs |
|---|---|---|---|
| O1 | Decision 2 allows only Itse's own Unix socket, but DAP `tcp` and `lsp_command` (kotlin-lsp's `start_debug_server` returns a port) need a loopback TCP connect. | (a) Connect only, 127.0.0.1/::1 only, enforced in code, never listen. (b) For a literal reading: a delegated `python3 -I` stdio↔TCP bridge, so the core opens no TCP socket (about 15 lines of extension data, one more process). Recommended: (a). Without either, the JVM debug proof has no transport. | owner, **by 11-18** (before S5) |
| O2 | stdio servers die with the daemon, so warm kotlin-lsp is lost on a daemon crash. | I1 kills leftovers and restarts cold with the flag set; results, runs, handles and snapshots persist. A per-server holder process is deferred. Does this meet EMA-188's "loses no state the agent needs"? | owner |
| O3 | Build-version mismatch. | Decided in this revision: a replaced binary (same proto, new exe inode) restarts the daemon automatically when no runs or debug sessions are live; otherwise the old daemon serves on, through `/proc/self/exe` (§3.3). | – |
| O4 | Which MCP era Claude Code 2.1.312 uses over stdio; whether 2026-07-28 keeps `initialize`. | S1 serves the 2025 versions; the modern era is added from the reserve after the `ITSE_E2E_CLAUDE` probe. | probe (S1) |
| O5 | MCP Tasks for long runs (EMA-188) vs polling. | Polling via `run:N` and `show_result` in I1; Tasks in I2. | – |
| O6 | Which real hosted MCP server proves the contract (D-X1). | Fixture in tests; in the box, `claude mcp serve` (opt-in, no download) or a small pinned community server in the JVM box. | owner |
| O7 | Line counting: code lines (comments free) or raw lines against 5K. | Code lines after rustfmt, raw printed. Raw lines would need about 15% more cuts. | owner |
| O8 | TOFU pins: policy and whether they are allowed at all in I1. | Refused unless a human runs the accept; no I1 package needs one. | owner |
| O9 | A team lock in a repo: how it combines with the machine lock. | Team pins seed packages the machine lock lacks; `status` shows differences; the machine lock wins. | owner |
| O10 | Human-only verbs are advisory until Touch ID (I2). | Record `actor: tty\|non-tty`. Enough for I1, except possibly consent (O21). | owner |
| O11 | Cancel escalation kills a Gradle daemon started inside the run's group. | Keep the escalation; alternative: SIGINT only, plus `--force`. | owner |
| O12 | Static target: glibc `+crt-static` vs ADR G3's musl. | glibc static; musl needs a toolchain target download the owner hasn't approved. | owner |
| O13 | Fixture servers in the binary or outside. | Decided in this revision: the separate `itse-fixture` binary under `tests/`, not counted, not shipped (C12). | – |
| O14 | SARIF is in ADR decision 13 but no I1 extension emits it. | I2, with the first linter extension (ktlint or detekt, if the repo uses one). | owner |
| O15 | kotlin-lsp facts: consent flags, exit codes 7 and 11, whether it pushes diagnostics for unopened files, whether it watches files itself (`"watch": "client"`), whether position queries need `didOpen`, the `start_debug_server` result format, RSS with Gradle and Kotlin daemons in a 14 GB box. | P-JVM, half a day. | a JVM box with the monorepo, **by 11-18** |
| O16 | rust-analyzer gave no diagnostics within 38 s after `didChangeWatchedFiles` + `didSave` with dynamic watch registration on. | Not investigated; the default (no client watching) works in ~0.2 s. | – |
| O17 | Bench: Claude Code clean config and credentials (`CLAUDE_CONFIG_DIR` vs `--bare` with an API key), permission mode for unattended trials, the spend cap, one $0.05 run to confirm the json-last pointers, Harbor's file names. | Fake adapter until settled; `without` arm before dogfood. | owner, by 11-25 |
| O18 | Linear text is stale in places: EMA-189 (8K lines, 400 per adapter, "CI fails a PR"), EMA-195 (`doctor`), EMA-181 G1/G3 ("no network", musl), EMA-183 (Harbor in I0). | Flag to the owner; this phase writes nothing to Linear. | owner |
| O19 | `cargo:test` results come from text matchers, which can break if libtest's panic line format changes. | Accept for I1: the `(PID)` group is optional, the golden input is real 1.92 output, the toolchain is pinned; libtest JSON needs nightly. | – |
| O20 | Wait defaults: CLI `--wait` 100 s, MCP `wait` 50 s; Claude Code's `MCP_TOOL_TIMEOUT` default is unverified. | Check in the S1 probe. | probe (S1) |
| O21 | Advisory `consent` lets an agent accept a vendor EULA and a data-sharing choice through Bash. Unlike trust, that is a legal act. | Refuse non-tty callers for `consent` only, in I1 (a few lines, no Touch ID); trust and the rest stay advisory. | owner |
| O22 | The DAP fallback if neither kotlin-lsp's `start_debug_server` nor a pinned adapter works by 12-04. | Fixture-proven DAP (stdio and TCP) in I1, the JVM adapter in I2 with sign-off (§13). | owner sign-off if it happens |
| O23 | The S0 re-baseline (§11.2): real models cost 447 lines more than their sketches, so the caps sum to 5,188 even after the whole cut list, 188 over the 5,000 gate with no reserve. | Pick one of the options under the §11.2 table, or accept that the reserve is gone and the later slices must fit in 5,000 real lines (the gate checks real lines, not caps). | owner, **before S1** |


## 15. Review notes (2026-11-04)

Two adversarial reviews were applied in place. Every fatal and major finding is fixed; the minor ones are fixed unless listed here. Where this design departs from a reviewer's suggested fix, the reason is given.

**Fixed as suggested, in short**: the detected layer and `use`/`unuse` (§4.1, §4.3); repo-keyed developer layer, trust and lock, so worktrees inherit them (§3.1); fully qualified pointers and harness identity as extension data, hashed (§3.4, §6.2); `integrate` human-only, MCP registration delegated to `claude mcp add-json`, idempotent recipe (§7.5); the hook never starts the daemon or a server and takes its workspace from the edited file (§3.3, §7.5); the trust hash leaves out the lock and versions (§10); the list cap covers exposed tools and `instructions`, with an allowlist (§5.7, §7.4); `debug` off MCP by default (§7.4); `args` on every tool, `lock` and `use` in the run tool, `grep`/`tail` on show (§6.1, §7.4); written-out `instructions` and keyword names (§7.4); a default stop rule for `debug test` (§5.5); a bench `with` arm that is declared and trusted, with a private bin dir (§9); the JVM probe moved to 11-18 with a named fallback (§13); per-target crate gate with build deps (§12.1); S0 re-baselines caps on real formatted types (§11.2); `/proc/self/exe` for re-exec (§5.6); one lock path, ordered shutdown, lock wait, socket re-bind, hello timeout (§3.3); a phase/live split of slot state and a poison-tolerant lock helper (§5.2); `git --no-optional-locks` (§5.4); bounded memory (§5.1, §5.4, §6.4); fixtures as a second binary under `tests/` (C12); every minor item on exit codes, cargo flags and matchers, ext over MCP, salted ids, per-append usage writes, server env, polyglot defaults, the outside-server note, curl edge cases, VS Code UI keys, zombies, relay errors, `daemon` rows, e2e env and cleanup, bench cleanup, gate time and the usage-cost gate.

**Departures and why:**
1. *Socket location* (R1 minor, R2 major disagree): R1 put the socket and lock in `~/.local/state`, R2 in a fixed `/tmp` path. Both goals hold with R1's layout: the path follows only the state dir (never `XDG_RUNTIME_DIR`), which decision 8 names, and `/tmp` is used only when sun_path would overflow.
2. *Repo key*: read from `.git` and `commondir` files instead of running `git rev-parse --git-common-dir`. No process per call, and it works without git on PATH; a test compares the two.
3. *`use` is not human-only.* It only declares what to use from extensions already on the machine and starts nothing; running project code still needs a human's `trust`. Decision 6's human-only list doesn't include it.
4. *Detection is extension data* (`detect` file names), not a core table, so the core keeps no language names (C9's rule). Its lines come from raised `manifest` and `verbs` caps, paid for by the cuts in §11.2, not from inside the old caps.
5. *MCP at local scope, no lazy relay* (R1 offered either). Registering per root already keeps `itse mcp` and its daemon out of unrelated sessions. A relay that answers `initialize` and `tools/list` itself would duplicate the list logic, exposed tools included, in a second place.
6. *Untrusted rust-analyzer is not started* (R1 offered either). One rule for every `executes_project_code` entry matches §4.4 and is fewer lines. A limited untrusted mode is I2, if usage shows the wait for trust costs agents.
7. *Unsettled hook prints nothing* rather than a `pending:` line. A line on every slow edit is context noise; not advancing the baseline already guarantees the errors are reported by the next settled call. The `settled` usage key gives the data to tune the deadline.
8. *Trust keeps the last 8 accepted hashes.* Not in the reviews; without it a switch between two branches with different team layers would ask each time.
9. *A built-in entry whose command changes does revoke trust.* Only version numbers and settings are excluded. Changed commands are new code to run, so asking again is correct.
10. *Accept-loop wake by self-connect* instead of a nonblocking poll (R2 offered both). Polling every 100–250 ms would add that much latency to the first connection of each CLI call.
11. *Fixture binary at `tests/fixture/main.rs`* rather than `src/bin/`: under `tests/` it is test code by the ordinary reading, so no counting exception is needed and O13 is closed instead of asked.
12. *JUnit is streamed* rather than capped at 4 MB, so large reports still parse with bounded memory.
13. *The repo-write reading* (R2 asked to confirm it with the owner) is stated in §0.1 rather than asked: decision 8 says Itse never writes into client repos, and the alternative (Itse may not run `cargo test`) would contradict I1's own acceptance. The owner can overrule it.
14. *Consent refusing non-tty callers* is left to the owner (O21), as R1 suggested, because decision 6 made human-only actions advisory.
15. *JVM probe date*: R2 asked for the week of 11-10, R1 for about 11-18. The plan says by 11-18, since the box is the owner's to provide; earlier is better.
