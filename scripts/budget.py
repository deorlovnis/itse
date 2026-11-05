#!/usr/bin/env python3
"""Itse's budget gates (design §11, §12.1). They run locally and fail closed.

usage: python3 -I scripts/budget.py budget.toml   run the gates against this repo
       python3 -I scripts/budget.py --self-test   plant one fault per gate in a scratch repo
                                                  and check that exactly that gate fails

Each gate prints `PASS <gate> <value>/<limit> (<seconds> s)` or `FAIL <gate>: <reason>`. A missing
tool, an exception or output that doesn't parse is a FAIL, never a pass. Gates that need the
toolchain print SKIP when the toolchain gate failed; the run fails either way.
"""

import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

PT_INTERP = 3


class Fail(Exception):
    """A gate's verdict, with the reason as its message."""


class World:
    """What the gates read: the repo, the tools on PATH, /proc and the scripted session.
    The self-test builds a planted one in a scratch dir."""

    def __init__(self, root, budget, path, proc=Path("/proc"), session=None):
        self.root, self.budget, self.path, self.proc = Path(root), budget, path, Path(proc)
        self.session = session or real_session
        self.info = print

    def which(self, tool):
        return shutil.which(tool, path=self.path) if self.path else None

    def run(self, argv, env=None):
        exe = self.which(argv[0])
        if not exe:
            raise Fail(f"{argv[0]} is not on PATH")
        done = subprocess.run([exe, *argv[1:]], cwd=self.root, capture_output=True, text=True, env={**os.environ, "PATH": self.path, **(env or {})})
        if done.returncode != 0:
            last = (done.stderr.strip().splitlines() or ["no output"])[-1]
            raise Fail(f"{' '.join(argv)} exited {done.returncode}: {last}")
        return done.stdout


def planted_result(n_items):
    """A stored query result as the daemon writes it (`results/<n>.json`), with `n_items` test items."""
    items = [{"id": f"T{i}", "kind": "test", "loc": {"path": "src/lib.rs", "line": i, "col": 5}, "src": [[i, "    assert_eq!(f(), 1);"]],
              "msg": f"tests::case_{i}", "detail": ["assertion `left == right` failed", f"  left: {i}"], "sources": []} for i in range(1, n_items + 1)]
    return {"itse": 1, "session": "cli", "verb": "test", "verdict": "fail", "scope": "itse", "counts": f"{n_items} failed", "cost": {"ms": 1200, "warm": None},
            "id": "q:1", "items": items, "states": [], "notes": [], "full": {"bytes": 0, "lines": 0}, "page": {"n": 1, "of": 1}}


def real_session(world):
    """The scripted session (design §12.2) so far: the release-static itse as the daemon of a scratch
    git workspace with a private HOME and XDG dirs; 200 CLI calls (status, and show with a page,
    grep, tail and a bare item id against a stored result); then `itse mcp` with initialize,
    tools/list and 20 tools/call. The fixture servers join it with S3 and S4. Returns the daemon's
    pid, still running; `world.stop_session()` stops it and removes the scratch dir."""
    itse = world.root / "target/budget" / host_triple(world) / "release" / world.budget["size"]["bin"]
    tmp = Path(tempfile.mkdtemp(prefix="itse-rss-", dir="/tmp"))
    env = {k: v for k, v in os.environ.items() if not k.startswith(("ITSE_", "XDG_", "CLAUDE_"))}
    env.update({"HOME": str(tmp / "home"), "ITSE_DAEMON_IDLE_S": "60", **{f"XDG_{d}_HOME": str(tmp / d.lower()) for d in ("CONFIG", "DATA", "STATE", "CACHE")}})
    work = tmp / "work"
    (work / ".git").mkdir(parents=True)

    def itse_run(*args):
        done = subprocess.run([str(itse), *args], cwd=work, env=env, capture_output=True, text=True)
        if done.returncode != 0:
            raise Fail(f"itse {' '.join(args)} exited {done.returncode}: {(done.stdout + done.stderr).strip().splitlines()[-1:]}")
        return done.stdout

    def stop():
        subprocess.run([str(itse), "daemon", "stop"], cwd=work, env=env, capture_output=True)
        shutil.rmtree(tmp, ignore_errors=True)

    world.stop_session = stop
    itse_run("status")
    ws = hashlib.sha256(os.path.realpath(work).encode()).hexdigest()[:12]
    results = tmp / "state/itse/ws" / ws / "results"
    results.mkdir(parents=True, exist_ok=True)
    (results / "1.json").write_text(json.dumps(planted_result(60)))
    calls = [["status"], ["show", "q:1"], ["show", "q:1", "--page", "2"], ["show", "q:1", "--grep", "case_1"], ["show", "q:1", "--tail", "20"], ["show", "T3"]]
    for i in range(198):
        itse_run(*calls[i % len(calls)])
    mcp_calls = mcp_session(itse, work, env)
    m = re.search(r"daemon pid (\d+)", itse_run("status"))
    if not m:
        raise Fail("itse status printed no daemon pid")
    pid = int(m.group(1))
    kb = vmhwm_kb(world.proc / str(pid) / "status")
    history = world.root / "target/budget/history.jsonl"
    with open(history, "a") as f:
        f.write(json.dumps({"at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "gate": "rss", "vmhwm_kb": kb, "cli_calls": 200, "mcp_calls": mcp_calls}) + "\n")
    return pid


def mcp_session(itse, work, env):
    """`itse mcp` as a harness drives it: initialize, tools/list and 20 tools/call; the calls made."""
    relay = subprocess.Popen([str(itse), "mcp"], cwd=work, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    tools = [("problems_symbols", {"verb": "status"}), ("show_result", {"id": "q:1"}), ("show_result", {"id": "q:1", "page": 2}), ("show_result", {"id": "q:1", "grep": "case_2"})]
    requests = [("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "budget", "version": "1"}}), ("tools/list", {})]
    requests += [("tools/call", {"name": name, "arguments": args}) for name, args in (tools[i % len(tools)] for i in range(20))]
    try:
        for n, (method, params) in enumerate(requests, 1):
            relay.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
            relay.stdin.flush()
            reply = json.loads(relay.stdout.readline() or "null")
            if not reply or reply.get("id") != n or "error" in reply or (reply["result"].get("isError") and method == "tools/call"):
                raise Fail(f"itse mcp: {method} got {reply!r}")
            if method == "initialize":
                relay.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    finally:
        relay.stdin.close()
        if relay.wait(timeout=10) != 0:
            raise Fail(f"itse mcp exited {relay.returncode}: {relay.stderr.read().strip()}")
    return len(requests) - 2


# --- gates ---


def gate_toolchain(w):
    want = w.budget["toolchain"]
    for tool in ("rustc", "cargo"):
        if not w.which(tool):
            raise Fail(f"{tool} is not on PATH")
    got = w.run(["rustc", "-V"]).strip()
    if not got.startswith(f"rustc {want} "):
        raise Fail(f"{got!r} is not rustc {want}")
    return want, want


def host_triple(w):
    m = re.search(r"^host: (\S+)$", w.run(["rustc", "-vV"]), re.M)
    if not m:
        raise Fail("rustc -vV printed no host line")
    return m.group(1)


def gate_size(w):
    cfg = w.budget["size"]
    triple = host_triple(w)
    w.run(["cargo", "build", "--locked", "--release", "--target", triple, "--target-dir", "target/budget", "--bin", cfg["bin"]], {"RUSTFLAGS": "-C target-feature=+crt-static"})
    path = w.root / "target/budget" / triple / "release" / cfg["bin"]
    size = path.stat().st_size
    if size > cfg["limit"]:
        raise Fail(f"{path.name} is {size} B, over {cfg['limit']} B")
    if cfg["static"] and has_interp(path.read_bytes()):
        raise Fail(f"{path.name} is dynamically linked (PT_INTERP); the release build must be +crt-static")
    return f"{size} B static", f"{cfg['limit']} B"


def has_interp(elf):
    """Whether an ELF file asks for a dynamic loader, read from its program headers."""
    if elf[:4] != b"\x7fELF" or elf[4] not in (1, 2) or elf[5] not in (1, 2):
        raise Fail("the binary is not an ELF file")
    end = "<" if elf[5] == 1 else ">"
    if elf[4] == 2:
        (phoff,), (phentsize, phnum) = struct.unpack_from(end + "Q", elf, 0x20), struct.unpack_from(end + "HH", elf, 0x36)
    else:
        (phoff,), (phentsize, phnum) = struct.unpack_from(end + "I", elf, 0x1C), struct.unpack_from(end + "HH", elf, 0x2A)
    return any(struct.unpack_from(end + "I", elf, phoff + i * phentsize)[0] == PT_INTERP for i in range(phnum))


def tree_names(text):
    names = set()
    for line in text.splitlines():
        line = line.replace(" (*)", "").replace(" (proc-macro)", "").strip()
        if line and not line.startswith("["):
            names.add(line.split()[0])
    if not names:
        raise Fail("cargo tree printed no crates")
    return names


def gate_crates(w):
    cfg = w.budget["crates"]
    allow, per = set(cfg["allow"]), {}
    for target in cfg["targets"]:
        per[target] = tree_names(w.run(["cargo", "tree", "--locked", "-e", "normal,build", "--prefix", "none", "--target", target]))
        if extra := per[target] - allow:
            raise Fail(f"{target} uses crates not in [crates].allow: {', '.join(sorted(extra))}")
    union = set().union(*per.values())
    if stale := allow - union:
        raise Fail(f"[crates].allow lists crates no target uses: {', '.join(sorted(stale))}")
    if len(union) > cfg["limit"]:
        raise Fail(f"{len(union)} crates, over {cfg['limit']}")
    counts = ", ".join(f"{t.split('-')[0]} {len(n)}" for t, n in per.items())
    return f"{len(union)} ({counts})", str(cfg["limit"])


def count_lines(path):
    """(code, raw) lines of a source file: code lines are non-blank and not comment-only. A
    file stops counting at its first column-0 #[cfg(test)]; tests.rs and *_tests.rs don't count."""
    if path.name == "tests.rs" or path.name.endswith("_tests.rs"):
        return 0, 0
    code = raw = 0
    for line in path.read_text().splitlines():
        if line.startswith("#[cfg(test)]"):
            break
        raw += 1
        stripped = line.strip()
        code += bool(stripped) and not stripped.startswith("//")
    return code, raw


def module_lines(src):
    """Lines per module: src/<m>.rs plus everything under src/<m>/."""
    mods = {}
    for path in sorted(src.rglob("*.rs")):
        name = path.relative_to(src).parts[0].removesuffix(".rs")
        code, raw = count_lines(path)
        old = mods.get(name, (0, 0))
        mods[name] = (old[0] + code, old[1] + raw)
    return mods


def gate_lines(w):
    cfg = w.budget["lines"]
    caps, mods = cfg["caps"], module_lines(w.root / "src")
    if unknown := sorted(set(mods) - set(caps)):
        raise Fail(f"no cap in budget.toml for module {', '.join(unknown)}")
    for name, (code, raw) in sorted(mods.items()):
        w.info(f"  lines {name:<10} {code:>4}/{caps[name]:<4} (raw {raw})")
    total, raw = sum(c for c, _ in mods.values()), sum(r for _, r in mods.values())
    if over := [f"{n} {c}/{caps[n]}" for n, (c, _) in sorted(mods.items()) if c > caps[n]]:
        # The owner (2026-11-05): the caps are a guide, not a wall. Only the total fails, unless
        # `caps_hard` is set (the self-test sets it to prove the gate still bites).
        if cfg.get("caps_hard", False):
            raise Fail(f"over its cap: {', '.join(over)}")
        w.info(f"  WARN over its cap (advisory): {', '.join(over)}")
    if total > cfg["limit"]:
        raise Fail(f"{total} code lines, over {cfg['limit']}")
    return f"{total} (raw {raw}; caps {sum(caps.values())})", str(cfg["limit"])


def gate_ext_lines(w):
    limit = w.budget["ext_lines"]["limit"]
    dirs = sorted(p for p in (w.root / "ext").glob("*") if p.is_dir()) + [p for p in [w.root / "bench/src"] if p.is_dir()]
    counts = {d: sum(1 for f in d.rglob("*") if f.is_file() for line in f.read_text().splitlines() if line.strip()) for d in dirs}
    if over := [f"{d.relative_to(w.root)} {n}" for d, n in counts.items() if n > limit]:
        raise Fail(f"over {limit} lines: {', '.join(over)}")
    return f"{max(counts.values(), default=0)} (largest of {len(dirs)})", str(limit)


def vmhwm_kb(status):
    m = re.search(r"^VmHWM:\s+(\d+) kB$", status.read_text(), re.M)
    if not m:
        raise Fail(f"no VmHWM line in {status}")
    return int(m.group(1))


def gate_rss(w):
    cfg = w.budget["rss"]
    if not cfg["enabled"]:
        # Off only before the daemon exists, so it can't stay off silently.
        if (w.root / "src/daemon.rs").exists():
            raise Fail("the rss gate is off but src/daemon.rs exists; set [rss] enabled = true")
        return "off until src/daemon.rs exists", f"{cfg['limit_kb']} kB"
    try:
        kb = vmhwm_kb(w.proc / str(w.session(w)) / "status")
    finally:
        getattr(w, "stop_session", lambda: None)()
    if kb > cfg["limit_kb"]:
        raise Fail(f"daemon VmHWM {kb} kB, over {cfg['limit_kb']} kB")
    return f"{kb} kB", f"{cfg['limit_kb']} kB"


GATES = [("toolchain", gate_toolchain), ("size", gate_size), ("crates", gate_crates), ("lines", gate_lines), ("ext-lines", gate_ext_lines), ("rss", gate_rss)]
NEEDS_TOOLCHAIN = {"size", "crates", "rss"}


def run_gates(w, out=print):
    """Run every gate; return the names of those that failed."""
    failed = []
    for name, gate in GATES:
        started = time.monotonic()
        if name in NEEDS_TOOLCHAIN and "toolchain" in failed:
            out(f"SKIP {name}: needs the toolchain")
            continue
        try:
            value, limit = gate(w)
            out(f"PASS {name} {value}/{limit} ({time.monotonic() - started:.1f} s)")
        except Fail as e:
            out(f"FAIL {name}: {e}")
            failed.append(name)
        except Exception as e:  # noqa: BLE001 - any surprise is a failure, never a pass
            out(f"FAIL {name}: {type(e).__name__}: {e}")
            failed.append(name)
    return failed


# --- self-test ---

ALLOW = ["itse", "anyhow", "cpu", "libc", "serde", "serde_derive"]
TREE = {"aarch64-unknown-linux-gnu": "itse v0.1.0 (/x)\nanyhow v1.0.0\ncpu v1 (*)\nlibc v0.2.0\nserde v1.0.0\nserde_derive v1.0.0 (proc-macro)\n", "x86_64-unknown-linux-gnu": "itse v0.1.0 (/x)\nanyhow v1.0.0\nserde v1.0.0 (*)\n"}


def elf(interp=False, size=0):
    """A little-endian ELF64 with one PT_LOAD, and a PT_INTERP when asked, padded to `size`."""
    phdrs = [1] + ([PT_INTERP] if interp else [])
    head = b"\x7fELF\x02\x01\x01" + bytes(9) + struct.pack("<HHIQQQIHHHHHH", 2, 0xB7, 1, 0, 64, 0, 0, 64, 56, len(phdrs), 0, 0, 0)
    body = head + b"".join(struct.pack("<IIQQQQQQ", t, 0, 0, 0, 0, 0, 0, 0) for t in phdrs)
    return body + bytes(max(0, size - len(body)))


def planted_world(tmp):
    """A scratch repo whose gates all pass, with fake rustc and cargo that serve canned output."""
    root, fake, bin_dir = tmp / "repo", tmp / "fake", tmp / "bin"
    for d in (root / "src/sys", root / "ext/rust", fake, bin_dir, tmp / "proc/4242"):
        d.mkdir(parents=True)
    (root / "src/main.rs").write_text("//! docs\n\nfn main() {}\n" + "#[cfg(test)]\nmod tests {\n" + "    // test\n" * 50 + "}\n")
    (root / "src/sys.rs").write_text("mod x;\n" + "pub fn f() {}\n" * 8)
    (root / "src/sys/x.rs").write_text("pub fn g() {}\n")
    (root / "src/sys/tests.rs").write_text("#[test]\nfn t() {}\n" * 40)
    (root / "ext/rust/itse-ext.json").write_text("{\n" * 10)
    (fake / "elf").write_bytes(elf(size=2000))
    for target, text in TREE.items():
        (fake / f"tree-{target}").write_text(text)
    (tmp / "proc/4242/status").write_text("Name:\titse\nVmPeak:\t  9000 kB\nVmHWM:\t    1700 kB\n")
    scripts = {
        "rustc": "import sys\nprint('rustc 1.92.0 (ded5c06cf 2025-12-08)')\nif '-vV' in sys.argv: print('host: aarch64-unknown-linux-gnu')\n",
        "cargo": f"import os, shutil, sys\nfrom pathlib import Path\nfake = Path({str(fake)!r})\na = sys.argv\n"
        "if a[1] == 'tree': print((fake / ('tree-' + a[a.index('--target') + 1])).read_text(), end='')\n"
        "elif a[1] == 'build':\n    out = Path(a[a.index('--target-dir') + 1]) / a[a.index('--target') + 1] / 'release'\n"
        "    out.mkdir(parents=True, exist_ok=True)\n    shutil.copy(fake / 'elf', out / a[a.index('--bin') + 1])\n"
        "else: sys.exit('fake cargo: ' + a[1])\n",
    }
    for name, text in scripts.items():
        (bin_dir / name).write_text(f"#!{sys.executable} -I\n{text}")
        (bin_dir / name).chmod(0o755)
    budget = {
        "toolchain": "1.92.0",
        "size": {"bin": "itse", "limit": 4096, "static": True},
        "crates": {"limit": 60, "targets": list(TREE), "allow": list(ALLOW)},
        "lines": {"limit": 100, "caps_hard": True, "caps": {"main": 5, "sys": 10}},
        "ext_lines": {"limit": 20},
        "rss": {"enabled": True, "limit_kb": 20480},
    }
    world = World(root, budget, str(bin_dir), tmp / "proc", session=lambda w: 4242)
    world.info = lambda line: None
    return world, fake


def plant_over_cap(w, fake):
    (w.root / "src/sys.rs").write_text((w.root / "src/sys.rs").read_text() + "pub fn h() {}\n" * 2)


def plant_unknown_module(w, fake):
    (w.root / "src/rogue.rs").write_text("fn r() {}\n")


def plant_unlisted_crate(w, fake):
    (fake / "tree-aarch64-unknown-linux-gnu").write_text(TREE["aarch64-unknown-linux-gnu"] + "left-pad v1.0.0\n")


def plant_stale_allow(w, fake):
    w.budget["crates"]["allow"].append("ghost")


def plant_big_elf(w, fake):
    (fake / "elf").write_bytes(elf(size=5000))


def plant_dynamic_elf(w, fake):
    (fake / "elf").write_bytes(elf(interp=True, size=2000))


def plant_big_ext(w, fake):
    (w.root / "ext/rust/itse-ext.json").write_text("{\n" * 21)


def plant_proc_status(w, fake):
    (w.proc / "4242/status").write_text("Name:\titse\nVmHWM:\t   30000 kB\n")


def plant_empty_path(w, fake):
    w.path = ""


def plant_daemon_while_rss_off(w, fake):
    w.budget["rss"]["enabled"] = False
    (w.root / "src/daemon.rs").write_text("fn d() {}\n")
    w.budget["lines"]["caps"]["daemon"] = 5


def plant_wrong_toolchain(w, fake):
    w.budget["toolchain"] = "1.93.0"


PLANTS = [
    ("over-cap module", plant_over_cap, "lines"),
    ("unknown module", plant_unknown_module, "lines"),
    ("unlisted crate", plant_unlisted_crate, "crates"),
    ("stale allowlist entry", plant_stale_allow, "crates"),
    ("oversized ELF", plant_big_elf, "size"),
    ("dynamic ELF", plant_dynamic_elf, "size"),
    ("oversized ext dir", plant_big_ext, "ext-lines"),
    ("fake /proc status", plant_proc_status, "rss"),
    ("empty PATH", plant_empty_path, "toolchain"),
    ("src/daemon.rs while rss is off", plant_daemon_while_rss_off, "rss"),
    ("another rustc version", plant_wrong_toolchain, "toolchain"),
]


def check_without_python(tmp):
    """check.sh must fail closed, before doing anything, when python3 is not on PATH."""
    check, bash = Path(__file__).with_name("check.sh"), shutil.which("bash")
    bin_dir = tmp / "nopython"
    bin_dir.mkdir()
    for tool in ("cargo", "rustfmt", "cargo-clippy"):
        if found := shutil.which(tool):
            (bin_dir / tool).symlink_to(found)
    done = subprocess.run([bash, str(check)], capture_output=True, text=True, env={**os.environ, "PATH": str(bin_dir)})
    if done.returncode == 0 or "FAIL tools: python3 is not on PATH" not in done.stdout:
        return f"check.sh without python3 exited {done.returncode}: {done.stdout.strip()!r}"
    return None


def self_test():
    started, problems = time.monotonic(), []
    with tempfile.TemporaryDirectory(prefix="itse-budget-") as tmp:
        clean, _ = planted_world(Path(tmp) / "clean")
        lines = []
        if failed := run_gates(clean, lines.append):
            problems.append(f"the clean world fails {failed}: {lines}")
        if any(line.startswith("SKIP") for line in lines):
            problems.append(f"the clean world skips a gate: {lines}")
        for i, (what, plant, gate) in enumerate(PLANTS):
            world, fake = planted_world(Path(tmp) / f"plant-{i}")
            plant(world, fake)
            lines = []
            failed = run_gates(world, lines.append)
            if failed != [gate]:
                problems.append(f"{what}: expected only {gate} to fail, got {failed}: {[l for l in lines if not l.startswith('PASS')]}")
            else:
                print(f"  plant {what}: only {gate} fails")
        if problem := check_without_python(Path(tmp)):
            problems.append(problem)
    for problem in problems:
        print(f"FAIL self-test: {problem}")
    total = len(PLANTS) + 2
    if problems:
        return 1
    print("  a clean world passes every gate; check.sh without python3 fails closed")
    print(f"PASS self-test {total}/{total} ({time.monotonic() - started:.1f} s)")
    return 0


def main(argv):
    if argv == ["--self-test"]:
        return self_test()
    if len(argv) != 1 or argv[0].startswith("-"):
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(argv[0]).resolve()
    with open(path, "rb") as f:
        budget = tomllib.load(f)
    started = time.monotonic()
    failed = run_gates(World(path.parent, budget, os.environ.get("PATH", "")))
    print(f"{'FAIL' if failed else 'PASS'} budget {len(GATES) - len(failed)}/{len(GATES)} gates ({time.monotonic() - started:.1f} s)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
