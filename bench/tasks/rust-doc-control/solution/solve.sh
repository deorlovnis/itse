#!/usr/bin/env bash
# The reference answer, run in the workspace: the units section.
set -euo pipefail
cd "${WORKSPACE:-.}"
cat >>README.md <<'DOC'

## Units

- `ms`: milliseconds
- `s`: seconds
- `m`: minutes
- `h`: hours
- `d`: days
DOC
