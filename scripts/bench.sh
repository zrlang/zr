#!/usr/bin/env bash
#
# zr benchmark runner.
#
# Builds the compiler (debug by default), compiles every benchmark at
# -O0/-O1/-O2/-O3/-Oz, runs each binary 5 times, and prints a Markdown table
# containing the best wall time per cell.
#
# Environment:
#   ZR_BIN=/path/to/zr     use an existing compiler binary (skips cargo build)
#   ZR_RELEASE=1           build the compiler with --release instead of debug
#   ZR_BENCH_RUNS=N        runs per cell (default 5)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OPTS=(-O0 -O1 -O2 -O3 -Oz)
BENCHES=(bench_fib bench_loop bench_sieve bench_string)
RUNS="${ZR_BENCH_RUNS:-5}"

if [ -n "${ZR_BIN:-}" ]; then
    ZR="$ZR_BIN"
    if [ ! -x "$ZR" ]; then
        echo "error: ZR_BIN is not executable: $ZR" >&2
        exit 1
    fi
elif [ "${ZR_RELEASE:-0}" = "1" ]; then
    echo "building compiler (release)..." >&2
    cargo build --quiet --release
    ZR="$ROOT/target/release/zr"
else
    echo "building compiler (debug)..." >&2
    cargo build --quiet
    ZR="$ROOT/target/debug/zr"
fi

VERSION="$("$ZR" -V | head -n 1)"
HOST="$(uname -s)-$(uname -m)"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/zr-bench.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# Print "<exit-code> <seconds>" for one invocation of $1.
# Uses /usr/bin/time when present, otherwise the bash `time` builtin.
measure() {
    local bin="$1" timing="$2" rc=0 t
    if [ -x /usr/bin/time ]; then
        /usr/bin/time -p "$bin" >/dev/null 2>"$timing" || rc=$?
        t="$(awk '$1 == "real" { print $2; exit }' "$timing")"
    else
        TIMEFORMAT='%R'
        { time "$bin" >/dev/null 2>/dev/null; } 2>"$timing" || rc=$?
        t="$(tail -n 1 "$timing")"
    fi
    if [ -z "$t" ]; then
        t="0"
    fi
    printf '%s %s\n' "$rc" "$t"
}

for bench in "${BENCHES[@]}"; do
    if [ ! -f "$ROOT/benchmarks/$bench.zr" ]; then
        echo "error: missing benchmark source: benchmarks/$bench.zr" >&2
        exit 1
    fi
done

echo "# zr benchmark results"
echo
echo "- compiler: \`$VERSION\`"
echo "- host: \`$HOST\`"
echo "- timing: best of $RUNS runs per cell, wall clock (seconds)"
echo
printf '| benchmark |'
for opt in "${OPTS[@]}"; do
    printf ' %s |' "$opt"
done
printf '\n|---|'
for _ in "${OPTS[@]}"; do
    printf '%s' '---:|'
done
printf '\n'

for bench in "${BENCHES[@]}"; do
    src="$ROOT/benchmarks/$bench.zr"
    row=""
    for opt in "${OPTS[@]}"; do
        bin="$TMP/${bench}${opt}"
        log="$TMP/${bench}${opt}.compile"
        if ! "$ZR" "$src" "$opt" -o "$bin" >"$log" 2>&1; then
            echo "error: failed to compile $bench at $opt" >&2
            cat "$log" >&2
            exit 1
        fi

        best=""
        for ((k = 0; k < RUNS; k++)); do
            timing="$TMP/${bench}${opt}.time"
            result="$(measure "$bin" "$timing")"
            rc="${result%% *}"
            t="${result##* }"
            if [ "$rc" -ne 0 ]; then
                echo "error: $bench exited $rc at $opt (run $((k + 1)) of $RUNS)" >&2
                exit 1
            fi
            if [ -z "$best" ]; then
                best="$t"
            else
                best="$(awk -v a="$best" -v b="$t" 'BEGIN { print (b + 0 < a + 0) ? b : a }')"
            fi
        done
        row="$row $(printf '%.3f' "$best") |"
    done
    printf '| %s |%s\n' "$bench" "$row"
done
