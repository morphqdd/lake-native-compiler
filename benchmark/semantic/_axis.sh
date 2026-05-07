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
    local bench_dir="$1" lang="$2" timeout_s="$3" expect="$4" expect_times="${5:-}"
    local bin="$bench_dir/build/$lang"
    [ -x "$bin" ] || { build_status "$lang" skip "no binary"; return; }
    local got
    got=$(timeout "$timeout_s" "$bin" 2>/dev/null || true)

    # Single-line snippet preview.
    local snippet="${got//$'\n'/ ⏎ }"
    snippet="${snippet:0:70}"
    [ -z "$got" ] && snippet="<empty / timeout>"

    local pass=0
    if [ -n "$expect_times" ]; then
        # count substring occurrences
        local count
        count=$(grep -c -F -- "$expect" <<<"$got" || true)
        [ "$count" = "$expect_times" ] && pass=1
        snippet="${count}× '$expect' (want $expect_times)  ${snippet}"
    else
        # regex match (single line — \n etc. NOT supported across lines)
        [[ "$got" =~ $expect ]] && pass=1
    fi

    if [ "$pass" = "1" ]; then
        printf "    ${GREEN}PASS${RESET} %-6s ${DIM}%s${RESET}\n" "$lang" "$snippet"
    else
        printf "    ${RED}FAIL${RESET} %-6s ${DIM}%s${RESET}\n" "$lang" "$snippet"
    fi
}

run_one() {
    local test="$1"
    if [ -n "$BENCH_FILTER" ]; then
        # Allow dash/underscore equivalence: `mailbox-isolation` ≡ `mailbox_isolation`.
        local f_norm="${BENCH_FILTER//-/_}"
        local t_norm="${test//-/_}"
        [ "$f_norm" != "$t_norm" ] && return 0
    fi
    local dir="$SEM_DIR/$test"
    [ -f "$dir/manifest.sh" ] || return 0

    local NAME="$test" DESC="" EXPECT="" EXPECT_TIMES="" LANGS="lake cpp go rust" TIMEOUT=3
    # shellcheck source=/dev/null
    source "$dir/manifest.sh"

    bench_header "$NAME" "$DESC"
    step "build"
    build_bench "$dir" "$LANGS" || { fail "all builds failed"; return; }

    [ "$BUILD_ONLY" = "1" ] && { echo; return; }

    if [ -n "$EXPECT_TIMES" ]; then
        step "assert  ${DIM}stdout contains '$EXPECT' ×$EXPECT_TIMES  timeout=${TIMEOUT}s${RESET}"
    else
        step "assert  ${DIM}stdout =~ /$EXPECT/  timeout=${TIMEOUT}s${RESET}"
    fi
    for lang in $LANGS; do
        run_lang "$dir" "$lang" "$TIMEOUT" "$EXPECT" "$EXPECT_TIMES"
    done
    echo
}

for d in "$SEM_DIR"/*/; do
    name=$(basename "$d")
    [ -f "$d/manifest.sh" ] || continue
    run_one "$name"
done
