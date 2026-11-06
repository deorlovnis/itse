#!/usr/bin/env bash
# setup.sh WS: the task's repo in WS, with the base committed.
set -euo pipefail
ws=$1
cp -R "$(dirname "$0")/repo/." "$ws"
cd "$ws"
git init -q -b main
git add -A
git -c user.name=itse-bench -c user.email=bench@localhost commit -q -m base
