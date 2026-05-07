#!/usr/bin/env bash
# Performance axis: time-to-completion benchmarks via hyperfine.

set -e

source "$SCRIPT_DIR/lib/ui.sh"
source "$SCRIPT_DIR/lib/build.sh"
source "$SCRIPT_DIR/lib/run.sh"

PERF_DIR="$SCRIPT_DIR/perf"

run_one() {
    local bench="$1"
    [ -n "$BENCH_FILTER" ] && [ "$BENCH_FILTER" != "$bench" ] && return 0
    local dir="$PERF_DIR/$bench"
    [ -d "$dir" ] || { warn "$bench: directory missing"; return 0; }
    local manifest="$dir/manifest.sh"
    [ -f "$manifest" ] || { warn "$bench: no manifest.sh"; return 0; }

    # Defaults — manifest can override.
    local NAME="$bench" DESC="" WARMUP=5 LANGS="lake cpp go rust"
    LABELS=()
    # shellcheck source=/dev/null
    source "$manifest"

    bench_header "$NAME" "$DESC"
    step "build"
    build_bench "$dir" "$LANGS" || { fail "all builds failed for $bench"; return 1; }

    [ "$BUILD_ONLY" = "1" ] && { echo; return 0; }

    step "run  ${DIM}hyperfine --warmup $WARMUP${RESET}"
    run_hyperfine "$dir" "$WARMUP" "$RESULTS/$bench.md" "${LABELS[@]}"
    echo
}

# Discover benches: any subdir of perf/ with a manifest.
for d in "$PERF_DIR"/*/; do
    name=$(basename "$d")
    [ "$name" = "build" ] && continue
    [ -f "$d/manifest.sh" ] || continue
    run_one "$name"
done
