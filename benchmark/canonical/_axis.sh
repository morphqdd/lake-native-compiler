#!/usr/bin/env bash
# Canonical axis: same task implemented in multiple languages, compared by
# **lines of source code that carry meaning** (excludes blank lines and
# single-line comments).  This is a productivity / expressiveness axis,
# not a performance axis — there is no timing.
#
# Each task is a directory under canonical/ with:
#   manifest.sh   — declares NAME, DESC, LANGS  (TASKDESC fields are optional)
#   <lang>.<ext>  — source files (one per language)
#                   Rust may use a `rust/` directory containing the standard
#                   Cargo layout; we count `rust/src/main.rs`.

set -e

source "$SCRIPT_DIR/lib/ui.sh"

CAN_DIR="$SCRIPT_DIR/canonical"
OUT_MD="$RESULTS/canonical.md"

# ── LoC counter ───────────────────────────────────────────────────────────────
#
# Strips:
#   * blank lines
#   * lines that start with `//` (after leading whitespace) — C/C++/Go/Rust/Lake
#   * lines that start with `#`  (after leading whitespace) — shell, .gitignore
#
# It does **not** strip multi-line `/* … */` comments.  Counted code is
# therefore an upper bound; for the benches we author we keep the source
# free of block comments so the count stays honest.
sloc() {
    local file="$1"
    [ -f "$file" ] || { echo 0; return; }
    awk '
        BEGIN { n = 0 }
        {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            if (line == "")            next
            if (line ~ /^\/\//)        next
            if (line ~ /^#/ && FILENAME ~ /\.(sh|toml|gitignore)$/) next
            n++
        }
        END { print n }
    ' "$file"
}

# Pick the canonical source path for a language inside a task dir.
src_path() {
    local task_dir="$1" lang="$2"
    case "$lang" in
        lake) echo "$task_dir/lake.lake" ;;
        go)   echo "$task_dir/go.go" ;;
        cpp)  echo "$task_dir/cpp.cpp" ;;
        c)    echo "$task_dir/c.c" ;;
        rust) echo "$task_dir/rust/src/main.rs" ;;
        *)    echo "" ;;
    esac
}

# ── output writer ─────────────────────────────────────────────────────────────
#
# We accumulate a single Markdown file with one row per task per language.
# The summary later in run.sh reads this file to produce the on-screen
# table.

mkdir -p "$RESULTS"
{
    echo "# Canonical task LoC"
    echo
    echo "Lines of source that carry meaning (blank lines and single-line"
    echo "comments are stripped).  Lower is better."
    echo
    echo "| task | lake | go | rust | cpp | c |"
    echo "|---|---:|---:|---:|---:|---:|"
} > "$OUT_MD"

# ── per-task loop ─────────────────────────────────────────────────────────────

for task_dir in "$CAN_DIR"/*/; do
    [ -d "$task_dir" ] || continue
    [ -f "$task_dir/manifest.sh" ] || continue
    [ -n "$BENCH_FILTER" ] && [ "$BENCH_FILTER" != "$(basename "$task_dir")" ] && continue

    NAME=""
    DESC=""
    # Subshell to avoid leaking task variables.
    eval "$(grep -E '^(NAME|DESC|LANGS)=' "$task_dir/manifest.sh")"
    NAME="${NAME:-$(basename "$task_dir")}"

    bench_header "$NAME" "$DESC"

    declare -A counts=( [lake]=- [go]=- [rust]=- [cpp]=- [c]=- )
    for lang in lake go rust cpp c; do
        src="$(src_path "$task_dir" "$lang")"
        if [ -n "$src" ] && [ -f "$src" ]; then
            counts[$lang]="$(sloc "$src")"
        fi
    done

    # Pretty per-task output.
    for lang in lake go rust cpp c; do
        v="${counts[$lang]}"
        [ "$v" = "-" ] && continue
        printf "    %-6s %4d\n" "$lang" "$v"
    done

    {
        printf "| %s | %s | %s | %s | %s | %s |\n" \
            "$NAME" "${counts[lake]}" "${counts[go]}" \
            "${counts[rust]}" "${counts[cpp]}" "${counts[c]}"
    } >> "$OUT_MD"
done

echo
echo -e "  ${DIM}wrote ${OUT_MD#$REPO_ROOT/}${RESET}"
