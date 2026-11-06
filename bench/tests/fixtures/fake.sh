#!/bin/sh
# A stand-in harness: `fake.sh --version`, or `fake.sh ARM TASK_DIR MCP_CONFIG INSTRUCTION` in
# the workspace. The with arm calls itse once and applies the task's solution; the without arm
# changes nothing tracked. Either way the last line is a Claude-shaped result.
set -eu
if [ "$1" = --version ]; then
    echo "fake 1.0"
    exit 0
fi
arm=$1 task=$2
echo "fake: $arm arm in $PWD"
# What the harness was given, next to its config dir, for the test to read.
cat >"$CLAUDE_CONFIG_DIR.seen" <<SEEN
mcp=$3
passed=${FAKE_PASSED:-}
blocked=${ANTHROPIC_API_KEY:-}
leaked=${GH_TOKEN:-}${SECRET_X:-}${OLDPWD:-}
itse=$(command -v itse || true)
cargo=$(command -v cargo || true)
gituser=$(git config user.name || true)
home=$HOME
SEEN
if [ "$arm" = with ]; then
    itse status >/dev/null 2>&1 || true
    WORKSPACE=$PWD bash "$task/solution/solve.sh"
else
    echo "looked, changed nothing" >NOTES.txt
fi
printf '{"type":"result","num_turns":3,"total_cost_usd":0.0125,"usage":{"input_tokens":1200,"cache_read_input_tokens":3400,"output_tokens":560}}\n'
