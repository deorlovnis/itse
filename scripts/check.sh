#!/usr/bin/env bash
# Itse's gates (design §12.1). They run locally before every commit; there is no CI. Each gate
# prints `PASS <gate> <value>/<limit> (<seconds> s)` or `FAIL <gate>: <reason>`, and any FAIL
# fails the run.
#
#   scripts/check.sh            fmt, clippy, tests, the budget self-test and the budget gates
#   scripts/check.sh --opt-in   the same, plus the #[ignore]d opt-in tests (ITSE_E2E_*)
set -euo pipefail

# Builtins only until every tool is known to be there, so a missing one fails here, closed.
for tool in cargo rustfmt cargo-clippy python3; do
    command -v "$tool" >/dev/null || { echo "FAIL tools: $tool is not on PATH"; exit 1; }
done
case "${1:-}" in
    "") opt_in=() ;;
    --opt-in) opt_in=(-- --include-ignored) ;;
    *) echo "usage: scripts/check.sh [--opt-in]" >&2; exit 2 ;;
esac
cd "$(dirname "${BASH_SOURCE[0]}")/.."
logs=target/check
mkdir -p "$logs"

now() { date +%s.%N; }
since() { python3 -I -c 'import sys; print(f"{float(sys.argv[2]) - float(sys.argv[1]):.1f}")' "$1" "$(now)"; }
started=$(now)

# gate NAME CMD...: run CMD with its output in target/check/NAME.log; a failure shows the log's tail.
gate() {
    local name=$1
    shift
    gate_started=$(now)
    if ! "$@" >"$logs/$name.log" 2>&1; then
        echo "FAIL $name: '$*' failed; the end of $logs/$name.log:"
        tail -n 40 "$logs/$name.log" | sed 's/^/  /'
        exit 1
    fi
}

# The tests' summary lines, summed: "<passed> <failed>". No summary at all is a failure.
test_counts() {
    grep -E '^test result: ' "$1" | sed -E 's/.* ([0-9]+) passed; ([0-9]+) failed.*/\1 \2/' |
        awk '{ p += $1; f += $2; n++ } END { if (n == 0) exit 1; print p, f }'
}

crates=(.)
[[ -f bench/Cargo.toml ]] && crates+=(bench)

for dir in "${crates[@]}"; do
    name=${dir/#./itse}
    gate "fmt-$name" cargo fmt --manifest-path "$dir/Cargo.toml" --check
    echo "PASS fmt-$name 0 files to reformat/0 ($(since "$gate_started") s)"

    gate "clippy-$name" cargo clippy --manifest-path "$dir/Cargo.toml" --locked --all-targets -- -D warnings
    warnings=$(grep -c '^warning' "$logs/clippy-$name.log" || true)
    if ((warnings > 0)); then
        echo "FAIL clippy-$name: $warnings warnings; see $logs/clippy-$name.log"
        exit 1
    fi
    echo "PASS clippy-$name $warnings warnings/0 ($(since "$gate_started") s)"

    gate "test-$name" cargo test --manifest-path "$dir/Cargo.toml" --locked "${opt_in[@]}"
    if ! counts=$(test_counts "$logs/test-$name.log"); then
        echo "FAIL test-$name: no 'test result:' line in $logs/test-$name.log"
        exit 1
    fi
    read -r passed failed <<<"$counts"
    echo "PASS test-$name $passed passed, $failed failed/0 failed ($(since "$gate_started") s)"
done

python3 -I scripts/budget.py --self-test
scripts/budget.sh
echo "PASS check all gates ($(since "$started") s)"
