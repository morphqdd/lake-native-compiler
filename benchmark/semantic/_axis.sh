#!/usr/bin/env bash
# Semantic axis: assertions about runtime behaviour (not just speed).
#
# Each test is a directory under semantic/ with:
#   manifest.sh   — declares NAME, DESC, EXPECT regex, LANGS, TIMEOUT
#   <lang>.<ext>  — implementations
#
# Each implementation is run with a timeout. If its stdout matches EXPECT,
# the test PASSES for that language. Otherwise FAIL.

set -e

source "$SCRIPT_DIR/lib/ui.sh"
source "$SCRIPT_DIR/lib/build.sh"

SEM_DIR="$SCRIPT_DIR/semantic"

run_lang() {
    local bench_dir="$1" lang="$2" timeout_s="$3" expect="$4"
    local bin="$bench_dir/build/$lang"
    [ -x "$bin" ] || { build_status "$lang" skip "no binary"; return; }
    local got
    got=$(timeout "$timeout_s" "$bin" 2>/dev/null || true)
    local snippet="${got:0:60}"
    [ -z "$snippet" ] && snippet="<empty / timeout>"
    if [[ "$got" =~ $expect ]]; then
        printf "    ${GREEN}PASS${RESET} %-6s ${DIM}%s${RESET}\n" "$lang" "$snippet"
    else
        printf "    ${RED}FAIL${RESET} %-6s ${DIM}got: %s${RESET}\n" "$lang" "$snippet"
    fi
}

run_one() {
    local test="$1"
    [ -n "$BENCH_FILTER" ] && [ "$BENCH_FILTER" != "$test" ] && return 0
    local dir="$SEM_DIR/$test"
    [ -f "$dir/manifest.sh" ] || return 0

    local NAME="$test" DESC="" EXPECT="" LANGS="lake cpp go rust" TIMEOUT=3
    # shellcheck source=/dev/null
    source "$dir/manifest.sh"

    bench_header "$NAME" "$DESC"
    step "build"
    build_bench "$dir" "$LANGS" || { fail "all builds failed"; return; }

    [ "$BUILD_ONLY" = "1" ] && { echo; return; }

    step "assert  ${DIM}stdout =~ /$EXPECT/  timeout=${TIMEOUT}s${RESET}"
    for lang in $LANGS; do
        run_lang "$dir" "$lang" "$TIMEOUT" "$EXPECT"
    done
    echo
}

for d in "$SEM_DIR"/*/; do
    name=$(basename "$d")
    [ -f "$d/manifest.sh" ] || continue
    run_one "$name"
done
