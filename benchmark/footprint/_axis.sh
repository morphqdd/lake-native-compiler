#!/usr/bin/env bash
# Footprint axis: binary size, dynamic dep count, cold-start time.
# Reuses binaries built by the perf axis (so requires perf to have run).

set -e

source "$SCRIPT_DIR/lib/ui.sh"
source "$SCRIPT_DIR/lib/run.sh"

PERF_DIR="$SCRIPT_DIR/perf"
OUT="$RESULTS/footprint.md"

# Pick a representative bench to measure footprint of each implementation.
# Default: cpu — every language has a binary there.
BENCH="${FOOTPRINT_BENCH:-cpu}"
DIR="$PERF_DIR/$BENCH"

if [ ! -d "$DIR/build" ]; then
    warn "footprint: $BENCH/build/ missing — run \`./run.sh perf $BENCH\` first"
    exit 0
fi

declare -A LABELS=(
    [seq]="c sequential"
    [lake]="lake (direct syscalls)"
    [cpp]="c++ (libstdc++)"
    [go]="go (static)"
    [rust]="rust (libc + libgcc)"
)

bench_header "binary size" "stripped, single binary"

# Header row + body sorted ascending by size for visual clarity.
declare -A SIZES
for lang in seq lake cpp go rust; do
    bin="$DIR/build/$lang"
    [ -f "$bin" ] || continue
    SIZES[$lang]=$(bin_size "$bin")
done

# Find max for proportional bars.
MAX=0
for s in "${SIZES[@]}"; do
    [ "$s" -gt "$MAX" ] && MAX=$s
done

# Print sorted ascending. Bash assoc arrays don't sort — emit, sort, read.
{
    for lang in "${!SIZES[@]}"; do
        echo "$lang ${SIZES[$lang]}"
    done
} | sort -k2 -n | while read -r lang size; do
    label="${LABELS[$lang]:-$lang}"
    fmt=$(fmt_size "$size")
    b=$(bar "$size" "$MAX" 28)
    ratio=$(awk "BEGIN { printf \"%.1f\", $size / ${SIZES[lake]:-1} }")
    printf "    %-7s %10s  %s  ${DIM}%sx vs lake${RESET}  ${DIM}%s${RESET}\n" \
        "$lang" "$fmt" "$b" "$ratio" "$label"
done

echo
bench_header "dynamic dependencies" "ldd output count"

for lang in seq lake cpp go rust; do
    bin="$DIR/build/$lang"
    [ -f "$bin" ] || continue
    n=$(dyn_deps "$bin")
    label="${LABELS[$lang]:-$lang}"
    if [ "$n" = "0" ]; then
        printf "    %-7s ${GREEN}%2d${RESET}  ${DIM}static / no deps${RESET}\n" "$lang" "$n"
    else
        printf "    %-7s    %2d  ${DIM}%s${RESET}\n" "$lang" "$n" "$label"
    fi
done

echo
warn "cold-start measurement TODO: needs minimal hello-world binary per lang \
(current bench reuses cpu workload → measures workload, not cold-start)"
echo
