#!/usr/bin/env bash
# The verifier, run from the task dir with WORKSPACE set: the code unchanged, and a units section
# in README.md that names every suffix parse_duration accepts. Exit 0 is a pass.
set -euo pipefail
cd "$WORKSPACE"
sha256sum --check --quiet <<'SUMS' || { echo "the code was changed"; exit 1; }
f24957e36ffc6af88183ca932d7d79015b6f518f44ccef286d01cf1f35e55925  src/lib.rs
a385acc5650451b2773f8de0832a611ae3f3e61cd3a64d5fea67de681838d8be  Cargo.toml
SUMS
section=$(awk '/^## Units/ { on = 1; next } /^## / { on = 0 } on' README.md)
[[ -n "$section" ]] || { echo "README.md has no '## Units' section"; exit 1; }
for unit in ms s m h d; do
    grep -qF "\`$unit\`" <<<"$section" || { echo "the units section misses \`$unit\`"; exit 1; }
done
