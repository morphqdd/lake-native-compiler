#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LAKEC="$REPO_ROOT/target/release/lakec"
BUILD="$SCRIPT_DIR/build"
mkdir -p "$BUILD"

# lakec resolves external/build/syscall.o relative to CWD — must run from repo root
lake_build() { (cd "$REPO_ROOT" && "$LAKEC" -r "$1"); }

# ── flags ──────────────────────────────────────────────────────────────────────
TELEGRAM=0
BENCH="all"   # all | io | cpu
for arg in "$@"; do
    case $arg in
        --telegram)  TELEGRAM=1 ;;
        --io)        BENCH="io" ;;
        --cpu)       BENCH="cpu" ;;
    esac
done

# ── colours ────────────────────────────────────────────────────────────────────
BOLD="\033[1m"
DIM="\033[2m"
CYAN="\033[36m"
GREEN="\033[32m"
YELLOW="\033[33m"
RESET="\033[0m"

header()  { echo -e "\n${BOLD}${CYAN}$1${RESET}"; }
ok()      { echo -e "  ${GREEN}✓${RESET} $1"; }
info()    { echo -e "  ${DIM}$1${RESET}"; }

fmt_size() {
    local bytes=$1
    if   [ "$bytes" -lt 1024 ];    then echo "${bytes} B"
    elif [ "$bytes" -lt 1048576 ]; then awk "BEGIN { printf \"%.1f KB\", $bytes/1024 }"
    else                                awk "BEGIN { printf \"%.1f MB\", $bytes/1048576 }"
    fi
}

bar() {
    local size=$1 max=$2 w=${3:-40}
    local filled=$(( size * w / max ))
    [ "$filled" -lt 1 ] && filled=1
    local b=""
    for ((i=0; i<filled; i++));  do b+="█"; done
    for ((i=filled; i<w; i++));  do b+="░"; done
    echo "$b"
}

# ── build: I/O bench ───────────────────────────────────────────────────────────
if [ "$BENCH" != "cpu" ]; then
    header "Building  ${DIM}[I/O async — 10 workers, write]${RESET}"

    info "lake  --release"
    lake_build "$SCRIPT_DIR/bench.lake" 2>/dev/null
    ok "lake"

    info "rust  cargo build --release"
    (cd "$SCRIPT_DIR/async_rust" && cargo build --release -q 2>/dev/null)
    ok "rust"

    info "c++   clang++ -O2 -std=c++20"
    clang++ -O2 -std=c++20 "$SCRIPT_DIR/async_cpp.cpp" -o "$BUILD/async_cpp_bench"
    ok "c++"
fi

# ── build: CPU bench ───────────────────────────────────────────────────────────
if [ "$BENCH" != "io" ]; then
    header "Building  ${DIM}[CPU async — 8 workers, fib(100k)]${RESET}"

    info "lake  --release"
    lake_build "$SCRIPT_DIR/cpu_bench.lake" 2>/dev/null
    ok "lake"

    info "c++   clang++ -O2 -std=c++20"
    clang++ -O2 -std=c++20 "$SCRIPT_DIR/cpu_bench_cpp.cpp" -o "$BUILD/cpu_bench_cpp"
    ok "c++"

    info "go    build (GOMAXPROCS=1)"
    go build -o "$BUILD/cpu_bench_go" "$SCRIPT_DIR/cpu_bench_go.go"
    ok "go"

    info "rust  cargo build --release (tokio current_thread)"
    (cd "$SCRIPT_DIR/cpu_bench_rust" && cargo build --release -q 2>/dev/null)
    ok "rust"

    info "c     clang -O2  (sequential baseline)"
    clang -O2 "$SCRIPT_DIR/cpu_seq_c.c" -o "$BUILD/cpu_seq_c"
    ok "c (seq)"
fi

# ── binary sizes ───────────────────────────────────────────────────────────────
if [ "$BENCH" != "cpu" ]; then
    header "Binary sizes  ${DIM}[I/O bench]${RESET}"

    LAKE_BIN="$BUILD/bench"
    RUST_BIN="$SCRIPT_DIR/async_rust/target/release/bench"
    CPP_BIN="$BUILD/async_cpp_bench"

    LAKE_SIZE=$(stat -c%s "$LAKE_BIN")
    RUST_SIZE=$(stat -c%s "$RUST_BIN")
    CPP_SIZE=$(stat -c%s "$CPP_BIN")

    MAX=$LAKE_SIZE
    [ "$RUST_SIZE" -gt "$MAX" ] && MAX=$RUST_SIZE
    [ "$CPP_SIZE"  -gt "$MAX" ] && MAX=$CPP_SIZE

    RUST_X=$(awk "BEGIN { printf \"%.1f\", $RUST_SIZE / $LAKE_SIZE }")
    CPP_X=$(awk  "BEGIN { printf \"%.1f\", $CPP_SIZE  / $LAKE_SIZE }")

    printf "  ${CYAN}%-8s${RESET}  %9s  ${CYAN}%s${RESET}  ${GREEN}1.0×${RESET}\n" \
        "lake" "$(fmt_size $LAKE_SIZE)" "$(bar $LAKE_SIZE $MAX)"
    printf "  %-8s  %9s  %s  %s×\n" \
        "c++"  "$(fmt_size $CPP_SIZE)"  "$(bar $CPP_SIZE  $MAX)"  "$CPP_X"
    printf "  %-8s  %9s  %s  %s×\n" \
        "rust" "$(fmt_size $RUST_SIZE)" "$(bar $RUST_SIZE $MAX)"  "$RUST_X"
fi

# ── benchmark: I/O ────────────────────────────────────────────────────────────
if [ "$BENCH" != "cpu" ]; then
    header "Benchmark  ${DIM}[I/O async — hyperfine --warmup 10]${RESET}"

    hyperfine \
        --warmup 10 \
        --shell none \
        --export-markdown "$SCRIPT_DIR/results_io.md" \
        --command-name "lake (cooperative, direct syscalls)" \
            "$BUILD/bench" \
        --command-name "rust (tokio current_thread)" \
            "$SCRIPT_DIR/async_rust/target/release/bench" \
        --command-name "c++ (coroutines, manual scheduler)" \
            "$BUILD/async_cpp_bench"
fi

# ── benchmark: CPU ────────────────────────────────────────────────────────────
if [ "$BENCH" != "io" ]; then
    header "Benchmark  ${DIM}[CPU async — 8 workers fib(100k) — hyperfine --warmup 5]${RESET}"

    LAKE_CPU="$BUILD/cpu_bench"
    CPP_CPU="$BUILD/cpu_bench_cpp"
    GO_CPU="$BUILD/cpu_bench_go"
    RUST_CPU="$SCRIPT_DIR/cpu_bench_rust/target/release/cpu_bench"
    C_SEQ="$BUILD/cpu_seq_c"

    hyperfine \
        --warmup 5 \
        --shell none \
        --export-markdown "$SCRIPT_DIR/results_cpu.md" \
        --command-name "c sequential (baseline)" \
            "$C_SEQ" \
        --command-name "lake (cooperative, quantum=256)" \
            "$LAKE_CPU" \
        --command-name "c++ (coroutines)" \
            "$CPP_CPU" \
        --command-name "rust (tokio current_thread)" \
            "$RUST_CPU" \
        --command-name "go (goroutines, GOMAXPROCS=1)" \
            "$GO_CPU"
fi

echo -e "\n  ${DIM}done${RESET}\n"
