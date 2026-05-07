#!/usr/bin/env bash
# Lake benchmark dispatcher.
#
# Usage:
#   ./benchmark/run.sh                # all axes
#   ./benchmark/run.sh perf            # only performance
#   ./benchmark/run.sh perf cpu        # one bench
#   ./benchmark/run.sh footprint
#   ./benchmark/run.sh semantic
#   ./benchmark/run.sh --build-only    # build everything, no timing
#
# Exit non-zero if any bench fails to build at least one implementation.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LAKEC="$REPO_ROOT/target/release/lakec"
RESULTS="$SCRIPT_DIR/results"
mkdir -p "$RESULTS"

source "$SCRIPT_DIR/lib/ui.sh"
source "$SCRIPT_DIR/lib/build.sh"
source "$SCRIPT_DIR/lib/run.sh"

# ── parse args ─────────────────────────────────────────────────────────────────

BUILD_ONLY=0
AXIS_FILTER=""
BENCH_FILTER=""
for arg in "$@"; do
    case "$arg" in
        --build-only) BUILD_ONLY=1 ;;
        perf|footprint|semantic|canonical) AXIS_FILTER="$arg" ;;
        *) BENCH_FILTER="$arg" ;;
    esac
done

# ── header ─────────────────────────────────────────────────────────────────────

echo
echo -e "  ${BOLD}Lake benchmarks${RESET}  ${DIM}— rev $(git rev-parse --short HEAD 2>/dev/null || echo '?')${RESET}"
echo -e "  ${DIM}runner: $(uname -sr)  cpus: $(nproc)  rustc: $(rustc --version 2>/dev/null | awk '{print $2}')${RESET}"

# ── ensure lakec is built ──────────────────────────────────────────────────────

if [ ! -x "$LAKEC" ]; then
    echo
    step "lakec missing — building (cargo build --release)"
    (cd "$REPO_ROOT" && cargo build --release -q)
fi

# ── axis dispatch ──────────────────────────────────────────────────────────────

run_axis() {
    local name="$1"
    [ -n "$AXIS_FILTER" ] && [ "$AXIS_FILTER" != "$name" ] && return 0
    local axis_dir="$SCRIPT_DIR/$name"
    [ -d "$axis_dir" ] || return 0
    local sh="$axis_dir/_axis.sh"
    [ -f "$sh" ] || { warn "axis $name has no _axis.sh — skipping"; return 0; }
    axis "$name"
    export BENCH_FILTER BUILD_ONLY SCRIPT_DIR REPO_ROOT LAKEC RESULTS
    bash "$sh"
}

run_axis perf
run_axis footprint
run_axis semantic
run_axis canonical

echo
echo -e "  ${DIM}done — results in $RESULTS/${RESET}"
echo
