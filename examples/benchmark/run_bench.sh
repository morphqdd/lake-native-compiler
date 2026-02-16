#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAKEC="$SCRIPT_DIR/../../target/release/lakec"
BUILD="$SCRIPT_DIR/build"
mkdir -p "$BUILD"

# ── flags ──────────────────────────────────────────────────────────────────────
TELEGRAM=0
for arg in "$@"; do
    case $arg in
        --telegram) TELEGRAM=1 ;;
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

# ── build ──────────────────────────────────────────────────────────────────────
header "Building"

info "lake  --release"
"$LAKEC" -r "$SCRIPT_DIR/bench.lake" 2>/dev/null
ok "lake"

info "rust  cargo build --release"
(cd "$SCRIPT_DIR/async_rust" && cargo build --release -q 2>/dev/null)
ok "rust"

info "c++   clang++ -O2 -std=c++20"
clang++ -O2 -std=c++20 "$SCRIPT_DIR/async_cpp.cpp" -o "$BUILD/async_cpp_bench"
ok "c++"

# ── size comparison ────────────────────────────────────────────────────────────
header "Binary sizes"

LAKE_BIN="$BUILD/bench"
RUST_BIN="$SCRIPT_DIR/async_rust/target/release/bench"
CPP_BIN="$BUILD/async_cpp_bench"

LAKE_SIZE=$(stat -c%s "$LAKE_BIN")
RUST_SIZE=$(stat -c%s "$RUST_BIN")
CPP_SIZE=$(stat -c%s "$CPP_BIN")

WIDTH=40
bar() {
    local size=$1 max=$2 w=${3:-$WIDTH}
    local filled=$(( size * w / max ))
    [ "$filled" -lt 1 ] && filled=1
    local b=""
    for ((i=0; i<filled; i++));  do b+="█"; done
    for ((i=filled; i<w; i++));  do b+="░"; done
    echo "$b"
}

MAX=$RUST_SIZE
[ "$CPP_SIZE"  -gt "$MAX" ] && MAX=$CPP_SIZE
[ "$LAKE_SIZE" -gt "$MAX" ] && MAX=$LAKE_SIZE

RUST_X=$(awk "BEGIN { printf \"%.1f\", $RUST_SIZE / $LAKE_SIZE }")
CPP_X=$(awk  "BEGIN { printf \"%.1f\", $CPP_SIZE  / $LAKE_SIZE }")

printf "  %-6s  %9s  %s\n" "" "" ""
printf "  ${CYAN}%-6s${RESET}  %9s  ${CYAN}%s${RESET}  ${GREEN}1.0×${RESET}\n" \
    "lake" "$(fmt_size $LAKE_SIZE)" "$(bar $LAKE_SIZE $MAX)"
printf "  %-6s  %9s  %s  %s×\n" \
    "c++"  "$(fmt_size $CPP_SIZE)"  "$(bar $CPP_SIZE  $MAX)"  "$CPP_X"
printf "  %-6s  %9s  %s  %s×\n" \
    "rust" "$(fmt_size $RUST_SIZE)" "$(bar $RUST_SIZE $MAX)"  "$RUST_X"
printf "\n  ${DIM}lake is ${RESET}${BOLD}${GREEN}${RUST_X}×${RESET}${DIM} smaller than rust"
printf " and ${RESET}${BOLD}${GREEN}${CPP_X}×${RESET}${DIM} smaller than c++${RESET}\n"

# ── benchmark ──────────────────────────────────────────────────────────────────
header "Benchmark  ${DIM}(hyperfine --warmup 10)${RESET}"

hyperfine \
    --warmup 10 \
    --shell none \
    --export-markdown "$SCRIPT_DIR/results.md" \
    --command-name "lake (cooperative, direct syscalls)" \
        "$LAKE_BIN" \
    --command-name "rust (tokio current_thread)" \
        "$RUST_BIN" \
    --command-name "c++ (coroutines, manual scheduler)" \
        "$CPP_BIN"

# ── append size section to results.md (after hyperfine overwrites it) ──────────
LAKE_BAR=$(bar $LAKE_SIZE $MAX)
CPP_BAR=$(bar $CPP_SIZE  $MAX)
RUST_BAR=$(bar $RUST_SIZE $MAX)

cat >> "$SCRIPT_DIR/results.md" <<EOF

## Binary sizes

\`\`\`diff
+ lake    $(printf "%9s" "$(fmt_size $LAKE_SIZE)")  $LAKE_BAR   1.0×
! c++     $(printf "%9s" "$(fmt_size $CPP_SIZE)")  $CPP_BAR   ${CPP_X}×
- rust    $(printf "%9s" "$(fmt_size $RUST_SIZE)")  $RUST_BAR   ${RUST_X}×
\`\`\`

> lake is **${RUST_X}×** smaller than rust and **${CPP_X}×** smaller than c++
EOF

# ── telegram export ────────────────────────────────────────────────────────────
if [ "$TELEGRAM" -eq 1 ]; then
    UNIT=$(awk -F'|' 'NR==1 { match($3,/\[([^]]+)\]/,a); print a[1] }' "$SCRIPT_DIR/results.md")
    LAKE_T=$(awk -F'|' 'NR==3 { gsub(/ /,"",$3); split($3,a,"±"); printf "%.0f", a[1] }' "$SCRIPT_DIR/results.md")
    RUST_T=$(awk -F'|' 'NR==4 { gsub(/ /,"",$3); split($3,a,"±"); printf "%.0f", a[1] }' "$SCRIPT_DIR/results.md")
    CPP_T=$(awk  -F'|' 'NR==5 { gsub(/ /,"",$3); split($3,a,"±"); printf "%.0f", a[1] }' "$SCRIPT_DIR/results.md")
    RUST_REL=$(awk -F'|' 'NR==4 { gsub(/ /,"",$6); split($6,a,"±"); printf "%.1f", a[1] }' "$SCRIPT_DIR/results.md")
    CPP_REL=$(awk  -F'|' 'NR==5 { gsub(/ /,"",$6); split($6,a,"±"); printf "%.1f", a[1] }' "$SCRIPT_DIR/results.md")

    W=20
    T_MAX=$CPP_T
    [ "$RUST_T" -gt "$T_MAX" ] && T_MAX=$RUST_T

    TG="$SCRIPT_DIR/results_telegram.txt"
    cat > "$TG" <<EOF
\`\`\`
Speed ($UNIT):
  lake  $(printf "%6s" "$LAKE_T")  $(bar $LAKE_T $T_MAX $W)  1.0×
  rust  $(printf "%6s" "$RUST_T")  $(bar $RUST_T $T_MAX $W)  ${RUST_REL}×
  c++   $(printf "%6s" "$CPP_T")  $(bar $CPP_T  $T_MAX $W)  ${CPP_REL}×

Size (release):
  lake  $(printf "%8s" "$(fmt_size $LAKE_SIZE)")  $(bar $LAKE_SIZE $MAX $W)  1.0×
  c++   $(printf "%8s" "$(fmt_size $CPP_SIZE)")  $(bar $CPP_SIZE  $MAX $W)  ${CPP_X}×
  rust  $(printf "%8s" "$(fmt_size $RUST_SIZE)")  $(bar $RUST_SIZE $MAX $W)  ${RUST_X}×
\`\`\`
EOF
    echo -e "  ${DIM}telegram format saved to results_telegram.txt${RESET}"
fi

echo -e "\n  ${DIM}results saved to results.md${RESET}\n"
