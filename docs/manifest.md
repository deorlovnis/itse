# The itse manifest

Itse reads what a workspace uses from small JSON files. It writes nothing into the repo: every file it writes lives under `~/.config/itse`, `~/.local/share/itse`, `~/.local/state/itse` or `~/.cache/itse` (`$XDG_*_HOME/itse` when set). This page describes the formats; `docs/design.md` §4 has the reasons. Every fenced `json` block below is parsed against the model by a test.

## Extensions

An extension is one JSON file, data only. It declares packages, servers, actions, problem matchers, consents, detection files, project pins, skills and harness data. Itse ships seven built-ins, compiled in from `ext/<id>/itse-ext.json`:

| Id | What it declares |
|---|---|
| `rust` | rust-analyzer and cargo, found on PATH (`install: path`); detected by `Cargo.toml` |
| `node` | node and npm, found on PATH; detected by `package.json` |
| `jdk` | Temurin, fetched (`install: fetch`), pinned through GitHub release digests; pulled in by `requires` |
| `kotlin` | kotlin-lsp, fetched, pinned through Open VSX `server-bundle.json`; requires `jdk` |
| `gradle` | the project's own `gradlew` actions; requires `jdk` |
| `bun` | bun, fetched, pinned through GitHub release digests |
| `claude-code` | caller identity and integration data for Claude Code; nothing to run |

A built-in carries launch data only, never pins: pins live in a lock. A built-in is on offer, not in use, until a layer names it in `use`. `itse ext list` lists them, `itse ext show <id>` shows what one declares.

A minimal extension:

```json
{ "itse_ext": 1, "id": "hello", "version": "0.1.0", "detect": ["hello.toml"],
  "packages": { "hello-ls": { "purl": "pkg:generic/example/hello-ls", "install": "path", "bin": { "hello-ls": "hello-ls" } } },
  "servers": [ { "id": "hello-ls", "protocol": "lsp", "transport": "stdio", "command": ["${pkg:hello-ls}/hello-ls", "--stdio"],
                 "filetypes": ["hello"], "languages": { "hello": "hello" } } ],
  "project_pins": [ { "name": "hello-version", "file": ".hello-version" } ] }
```

A fetched package names its pin source and the hosts its bytes come from. The pins themselves are written by `itse lock --refresh`:

```json
{ "itse_ext": 1, "id": "kotlin", "version": "0.1.0", "requires": ["jdk"], "detect": ["settings.gradle.kts", "build.gradle.kts"],
  "packages": { "kotlin-lsp": {
      "purl": "pkg:generic/jetbrains/kotlin-lsp", "install": "fetch",
      "pin_source": { "kind": "openvsx-bundle", "extension": "JetBrains/kotlin-server", "platforms": { "linux_arm64": "linux-arm64", "linux_x64": "linux-x64" } },
      "archive": "tar.gz", "strip": 1, "bin": { "kotlin-lsp": "bin/intellij-server" },
      "hosts": ["open-vsx.org", "download.jetbrains.com"],
      "expires_after_days": 30, "exit_codes": { "7": "server_expired", "11": "needs_consent:eula" },
      "consents": [
        { "id": "eula", "kind": "hash", "of_file": "EULA.txt", "hex_prefix": 16, "flag": "--eula=" },
        { "id": "region", "kind": "choice", "env": "INTELLIJ_REGION" },
        { "id": "data_sharing", "kind": "choice", "env": "INTELLIJ_DATA_SHARING", "values": ["none", "anonymous", "full"] } ],
      "jvm_args": ["-Xmx2g"], "jvm_args_env": "IJ_JAVA_OPTIONS", "executes_project_code": true } },
  "servers": [ { "id": "kotlin-lsp", "protocol": "lsp", "transport": "stdio",
      "command": ["${pkg:kotlin-lsp}/bin/intellij-server", "--stdio", "--system-path=${cache}/kotlin-lsp", "${consent:eula}"],
      "filetypes": ["kt", "kts"], "env": { "JAVA_HOME": "${pkg:jdk}" } } ] }
```

Package keys: `purl`, `install` (`fetch` or `path`), `pin_source` (`github-release` with `repo` and `assets` per target, or `openvsx-bundle` with `extension` and `platforms`), `archive`, `strip`, `bin` (name → path inside the package), `hosts`, `expires_after_days`, `exit_codes`, `consents`, `jvm_args`, `jvm_args_env`, `executes_project_code`.

A server's `command`, `env`, `init_options` and `settings` take the same templates (`${pkg:jdk}`, `${cache}`, `${workspaceFolder}`, …); an unknown one is refused when the extension loads.

`project_pins` names files in the repo that pin a version the project manages itself. They are read for `status`, never rewritten: `keys` are JSON pointers in a `.json` file and keys in a `.properties` file; with no `keys`, the first line is shown.

### Actions are VS Code tasks

An action is a VS Code `tasks.json` entry, pasted as it is. VS Code's UI keys (`presentation`, `runOptions`, `detail`, `icon`, `hide`, `promptOnClose`, `isBackground`, `windows`, `osx`, `linux`) are accepted and ignored, and `itse ext show` lists them as `ignored`; so is `options.shell`, since an action runs its argv without a shell. Itse's additions are `target_args`, `filter_args`, `parse`, `frames`, `classify`, `counts`, `ready`, `drop_lines`, `outputs`, `notes`, `executes_project_code` and `timeout_s`. A task from the VS Code documentation:

```json
{ "label": "build", "type": "shell", "command": "msbuild",
  "args": ["/property:GenerateFullPaths=true", "/t:build", "/consoleloggerparameters:NoSummary"],
  "group": "build", "presentation": { "reveal": "silent" }, "problemMatcher": "$msCompile" }
```

### Matchers, reports and counts

`itse build`, `itse test` and `itse run` parse a run once it has ended; everything tool-specific is in the extension.

- `problemMatcher` is VS Code's, in a subset: single-line patterns and fixed multi-line patterns (each line matches the next pattern; no `loop`), with `regexp`, `file`, `line`, `column`, `severity`, `code` and `message`; `fileLocation` is `absolute`, `relative` or `["relative", dir]`, and a `file://` prefix is stripped. Named matchers live in an extension's `problemMatchers` and are referenced as `$name` from any extension. Itse's additions to a matcher: `kind: "test"` makes failed-test items (`T1`) named by the `name` group, with the message under them, and `detail_lines` keeps up to that many following lines until a blank one.
- Regexes are [regex-lite](https://docs.rs/regex-lite)'s: no lookaround and no backreferences (such a regex fails when the extension loads), and `\w`, `\d` and `\s` are ASCII-only.
- `drop_lines` removes matching lines before any matcher sees them; `counts` sums `passed`, `failed` and `skipped` groups over every line that matches (one per test binary); `notes` are fixed lines every result of the action carries.
- `parse: ["junit-xml:<glob>"]` reads JUnit XML reports (`**/` any directories, `*` within a name) written by this run, else the newest ones marked as written before it; `frames` and `classify` trim the failures' stack traces.

```json
{ "label": "cargo:test", "type": "process", "command": "${pkg:cargo}/cargo", "group": { "kind": "test", "isDefault": true },
  "args": ["test", "--tests", "--no-fail-fast", "--message-format=short", "${target_args}", "${filter_args}"],
  "target_args": ["-p", "${input:target}"], "filter_args": ["${input:filter}"], "problemMatcher": ["$rustc-short", "$libtest-panic"],
  "counts": { "regexp": "^test result: (?:ok|FAILED)\\. (\\d+) passed; (\\d+) failed; (\\d+) ignored", "passed": 1, "failed": 2, "skipped": 3 },
  "drop_lines": ["^note: run with .RUST_BACKTRACE"], "notes": ["doctests not run (I2)"], "outputs": ["target/", "Cargo.lock"], "executes_project_code": true }
```

### Templates

Strings in a server's `command` and `env` and in an action's `command`, `args`, `filter_args`, `target_args` and `options` may hold these fields: `${pkg:x}` (a package's root), `${consent:x}` (a recorded consent with its flag), `${workspaceFolder}`, `${input:x}`, `${cache}` (`~/.cache/itse`), `${target_args}`, `${filter_args}` and `${ready:N}`. Any other `${…}` fails when the extension loads.

### Unknown keys

Every model refuses unknown keys, so a typo is an error that names the file, not a setting that silently does nothing. The one exception is an action's VS Code UI keys, above.

## Layers

What a workspace uses comes from three layers:

- **detected**: an extension whose `detect` file sits at the root (top-level names only) is a suggestion. `status` shows it as `suggested` with `next (agent): itse use <id>`. Nothing is used until a layer names it.
- **team**: `.itse/itse.json` in the worktree. Read, never written. Its entries run only after `itse trust`, and its `system` commands don't run before then either.
- **developer**: one file per repo in `~/.config/itse/workspaces/<name>.json`, found by its `repo` field (the repo's git common dir), so every worktree of a clone shares it. `itse use` and `itse unuse` write it; a human may edit it.

The developer layer's entry replaces the team layer's entry of the same name; there is no deep merge. Built-in and added extensions supply the definitions that `use` names; `requires` pulls in more.

```json
{ "itse": 1, "repo": "/work/crm/.git",
  "use": { "jdk": { "version": "25.0.5+8" }, "kotlin": {}, "gradle": {}, "node": { "install": "path" } },
  "system": { "libheif": { "lib": "libheif.so.1" }, "docker": { "cmd": ["docker", "info"] } },
  "servers": { "kotlin-lsp": { "idle_minutes": 60 }, "some-mcp": { "expose": true, "tools": ["search", "fetch"], "max_list_bytes": 900 } },
  "inputs": [ { "id": "target", "type": "promptString" }, { "id": "filter", "type": "promptString" } ] }
```

A team layer has the same shape without `repo`:

```json
{ "itse": 1, "use": { "rust": {} }, "system": { "openssl": { "lib": "libssl.so.3" } } }
```

`system` checks are typed, never shell strings: `lib` looks for a library in the usual dirs, `cmd` is an argv that must exit 0 within 5 s. A failed check is `blocked` with `needs_image_layer` and actor `halti-owner`.

### `itse use` and `itse unuse`

```
itse use rust
itse use jdk version=25.0.5+8
itse use node install=path
itse unuse node
```

`use` adds or replaces `use.<ext>` in this repo's developer layer, creating it (0600, in a 0700 dir) with `repo` set; `unuse` removes the entry, and the file once nothing else is in it. The settings are `install=fetch|path` and `version=V`, kept as strings; `install=fetch` moves only the extension's packages that have a `pin_source`, the others keep their own install. Both run without the daemon, start nothing, and never change trust. An agent can declare over MCP (`test_build_run {verb: "use", args: ["rust"]}`); only a human trusts.

## States

`status` gives every item one state, with who acts next and the command:

| State | Meaning | Actor and next |
|---|---|---|
| `suggested` | detected from a file, not in any layer's `use` | agent: `itse use <ext>` |
| `ready` | in the store (or a project or system item that checks out), consented, trusted if needed | – |
| `unpinned` | a `path` install found on PATH; runs, without a pin | – |
| `missing` | declared, not pinned or not in the store, or not on PATH | agent: `itse lock --refresh <pkg>` or `itse install <pkg>` |
| `untrusted` | runs project code, and the workspace isn't trusted | human: `itse trust` |
| `needs_consent` | a declared consent isn't recorded | human: `itse consent <pkg>` |
| `expiring` | the pin's `expires_estimate` is within 7 days; still runs | agent: `itse lock --refresh <pkg>` |
| `expired` | past the estimate | agent: `itse lock --refresh <pkg>` |
| `blocked` | a system check failed | halti-owner |

Only `ready`, `unpinned` and `expiring` items start. `itse status --hosts` also lists the hosts the missing packages would be fetched from.

## The lock

Pins live in `~/.local/state/itse/repo/<rid>/itse.lock.json`, shared by the repo's worktrees and never written into the repo. A team lock at `.itse/itse.lock.json` has the same shape; its pins seed packages the machine lock doesn't have yet, once the workspace is trusted. It is a team-layer file: on its own it asks for `itse trust`, and until then a package only it pins shows `untrusted`.

```json
{ "itse_lock": 1,
  "packages": { "kotlin-lsp": { "purl": "pkg:generic/jetbrains/kotlin-lsp@271.2085.0", "install": "fetch",
      "targets": { "linux_arm64": { "url": "https://download.jetbrains.com/language-server/kotlin-server/271.2085.0/kotlin-server-271.2085.0-aarch64.tar.gz",
                                    "sha256": "877aee61dc96e", "size": null, "provenance": "upstream:openvsx-bundle" } },
      "published": "2026-11-04", "expires_estimate": "2026-12-04", "extension": "kotlin@0.1.0" } },
  "project_pins": { "gradle-wrapper": { "distributionSha256Sum": "present" } } }
```

## Trust, consent and added extensions

These are human-only. Over MCP they return `NEEDS_HUMAN` with the line to run in a terminal. On the CLI they are advisory in I1 and run, and the record keeps `actor: "tty"` or `"non-tty"`; `itse consent` alone refuses a caller without a terminal.

- **`itse trust [--revoke]`** accepts the workspace's trust hash for the repo, in `repo/<rid>/trust.json`. The hash covers the team-layer files' bytes and the unrendered `command`, `args`, `options.cwd` and `env` of every entry in use that runs project code. Pins, store paths, versions and settings stay out, so `lock --refresh`, an install or an itse upgrade keeps trust, while a team-layer change or a new project-code entry asks again. The last 8 accepted hashes are kept, so switching between branches doesn't ask each time.
- **`itse consent <pkg> [--eula-hash H] [--region R] [--data-sharing V]`** records a package's consents in `consents.json`. Nothing is defaulted: without the flags it names the ones it needs, and the EULA file's path and hash. A new EULA hash asks again.
- **`itse ext add <file>`** copies an extension to `~/.config/itse/extensions/<id>.json` and records its sha256 in `extensions.json`; a file that no longer has that hash isn't loaded. `itse ext remove <id>` removes it. Built-ins can't be removed.

`found.json` records each `path` install as found: its path and the binary's sha256, hashed again when the file changes.
