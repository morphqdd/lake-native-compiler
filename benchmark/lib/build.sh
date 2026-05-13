#!/usr/bin/env bash
# Per-language build helpers. Each builds into <bench_dir>/build/<lang>.
#
# Conventions:
#   bench_dir/lake.lake   ← Lake source     → bench_dir/build/lake
#   bench_dir/cpp.cpp     ← C++ coroutines  → bench_dir/build/cpp
#   bench_dir/go.go       ← Go (GOMAXPROCS=1) → bench_dir/build/go
#   bench_dir/rust/       ← Cargo crate (tokio) → bench_dir/build/rust
#   bench_dir/seq.c       ← C sequential baseline → bench_dir/build/seq
#
# Each function returns 0 on success, non-zero on failure.

build_lake() {
    local bench_dir="$1"
    local src="$bench_dir/lake.lake"
    [ -f "$src" ] || return 2  # not present = skip
    # Per-bench compile-time env (LAKE_UNROLL, LAKE_QUANTUM, …) lives in
    # <bench_dir>/lake.env so each bench captures its own tuning knobs.
    local env_file="$bench_dir/lake.env"
    # `+std.foo.{ … }` imports resolve via LAKE_PATH; default to the
    # sibling `lake-stdlib` checkout next to the compiler repo.  Bench
    # authors can override by exporting LAKE_PATH or by setting a
    # bench-local `lake.env`.
    local default_lake_path="$REPO_ROOT/../lake-stdlib"
    # lakec resolves embedded syscall.o relative to CWD — must run from REPO_ROOT
    (
        cd "$REPO_ROOT"
        if [ -z "${LAKE_PATH:-}" ] && [ -d "$default_lake_path" ]; then
            export LAKE_PATH="$default_lake_path"
        fi
        if [ -f "$env_file" ]; then
            set -a
            # shellcheck disable=SC1090
            . "$env_file"
            set +a
        fi
        "$LAKEC" -r "$src"
    ) >/dev/null 2>&1 || return 1
    # lakec writes to <src_dir>/build/<stem>; our stem is "lake".
    [ -x "$bench_dir/build/lake" ] || return 1
    return 0
}

build_cpp() {
    local bench_dir="$1"
    local src="$bench_dir/cpp.cpp"
    [ -f "$src" ] || return 2
    mkdir -p "$bench_dir/build"
    clang++ -O2 -std=c++20 "$src" -o "$bench_dir/build/cpp" 2>/dev/null || return 1
}

build_go() {
    local bench_dir="$1"
    local src="$bench_dir/go.go"
    [ -f "$src" ] || return 2
    mkdir -p "$bench_dir/build"
    go build -o "$bench_dir/build/go" "$src" 2>/dev/null || return 1
}

build_rust() {
    local bench_dir="$1"
    [ -d "$bench_dir/rust" ] || return 2
    (cd "$bench_dir/rust" && cargo build --release -q 2>/dev/null) || return 1
    mkdir -p "$bench_dir/build"
    # Locate produced binary — there's exactly one.
    local bin
    bin=$(find "$bench_dir/rust/target/release" -maxdepth 1 -type f -executable \
           ! -name '*.d' ! -name '*.rmeta' 2>/dev/null | head -1)
    [ -n "$bin" ] && [ -x "$bin" ] || return 1
    cp "$bin" "$bench_dir/build/rust"
}

build_seq() {
    local bench_dir="$1"
    local src="$bench_dir/seq.c"
    [ -f "$src" ] || return 2
    mkdir -p "$bench_dir/build"
    clang -O2 "$src" -o "$bench_dir/build/seq" 2>/dev/null || return 1
}

# Build all languages declared in $LANGS for a bench, printing per-lang status.
# Returns 0 if at least one build succeeded.
build_bench() {
    local bench_dir="$1"
    local langs="${2:-lake cpp go rust seq}"
    local any_ok=1
    for lang in $langs; do
        case "$lang" in
            lake) build_lake "$bench_dir" ;;
            cpp)  build_cpp  "$bench_dir" ;;
            go)   build_go   "$bench_dir" ;;
            rust) build_rust "$bench_dir" ;;
            seq)  build_seq  "$bench_dir" ;;
            *)    continue ;;
        esac
        local rc=$?
        case $rc in
            0) build_status "$lang" ok;   any_ok=0 ;;
            1) build_status "$lang" fail "build failed" ;;
            2) build_status "$lang" skip ;;
        esac
    done
    return $any_ok
}
