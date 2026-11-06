#!/usr/bin/env bash
# The reference fix, run in the workspace: the window's end is inclusive.
set -euo pipefail
cd "${WORKSPACE:-.}"
sed -i 's/&readings\[start\.\.end\]/\&readings[start..=end]/' src/lib.rs
