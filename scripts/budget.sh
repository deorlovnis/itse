#!/usr/bin/env bash
# The budget gates (design §12.1): toolchain, binary size, crates, lines, ext lines and rss.
# They fail closed: a missing tool or output that doesn't parse is a FAIL.
set -euo pipefail
command -v python3 >/dev/null || { echo "FAIL budget: python3 is not on PATH"; exit 1; }
cd "$(dirname "${BASH_SOURCE[0]}")/.."
exec python3 -I scripts/budget.py budget.toml
