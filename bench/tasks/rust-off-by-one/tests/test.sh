#!/usr/bin/env bash
# The verifier, run from the task dir with WORKSPACE set: nothing changed outside src/ (so no
# .cargo/config.toml or build.rs steers cargo), the tests passing, then a hidden test the agent
# never saw. Exit 0 is a pass.
set -euo pipefail
here=$(pwd)
cd "$WORKSPACE"
sha256sum --check --quiet <<'SUMS' || { echo "the tests or the manifest were changed"; exit 1; }
391e58400d9d4b019c4ca1a2fefa2da03b84daa2fa1409a0cf05c7eadfe76679  tests/window.rs
71af0307869c729becaa89a457385379718d13a731f8cbb5d6f1395a31f17229  Cargo.toml
4f864af6b07d4443fe62f84ba0c9547bb5245ebdce7b17849aae02600b67940a  Cargo.lock
SUMS
outside=$(git -c core.fsmonitor=false -c core.untrackedCache=false status --porcelain --ignored --untracked-files=all -- . ':!src' ':!target')
[[ -z "$outside" ]] || { echo "changed outside src/:"; echo "$outside"; exit 1; }
cargo test --offline --quiet
cp "$here/tests/hidden.rs" tests/hidden_window.rs
trap 'rm -f tests/hidden_window.rs' EXIT
cargo test --offline --quiet --test hidden_window
