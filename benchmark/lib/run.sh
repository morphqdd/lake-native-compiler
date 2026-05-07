#!/usr/bin/env bash
# Bench execution helpers built on hyperfine + custom probes.

# Run hyperfine over all built binaries in <bench_dir>/build/.
# Args:
#   bench_dir       e.g. perf/cpu
#   warmup          int, e.g. 5
#   results_md      path to dump markdown
#   "lake:Lake (cooperative)"  display labels per-lang, colon-separated
run_hyperfine() {
    local bench_dir="$1" warmup="$2" results_md="$3"
    shift 3
    local args=()
    for spec in "$@"; do
        local lang="${spec%%:*}"
        local label="${spec#*:}"
        local bin="$bench_dir/build/$lang"
        [ -x "$bin" ] || continue
        args+=( --command-name "$label" "$bin" )
    done
    [ ${#args[@]} -gt 0 ] || { warn "no binaries to run"; return 1; }
    hyperfine --warmup "$warmup" --shell none --export-markdown "$results_md" "${args[@]}"
}

# Validate a binary's stdout matches expected pattern before timing.
# Args: bin_path  expected_regex
validate_output() {
    local bin="$1" expected="$2"
    local got
    got=$(timeout 10 "$bin" 2>&1) || return 1
    [[ "$got" =~ $expected ]] || { fail "output mismatch: got ${got:0:80}…"; return 1; }
    return 0
}

# Get binary size in bytes (stripped); 0 if missing.
bin_size() {
    [ -f "$1" ] && stat -c%s "$1" || echo 0
}

# Get number of dynamic deps via ldd; -1 if static or missing.
dyn_deps() {
    local bin="$1"
    [ -x "$bin" ] || { echo -1; return; }
    local out
    out=$(ldd "$bin" 2>&1)
    if echo "$out" | grep -q "not a dynamic executable\|statically linked"; then
        echo 0
        return
    fi
    echo "$out" | grep -c "=>"
}

# Median / wall time in microseconds via /usr/bin/time.
# Bash 5+ printf "%T" not portable; use external `time -f`.
time_us() {
    local bin="$1"
    /usr/bin/time -f "%e" "$bin" 2>&1 1>/dev/null | awk '{ printf "%d", $1*1000000 }'
}
