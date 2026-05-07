#!/usr/bin/env bash
# Shared UI helpers for benchmark output.

# Colours / styles
BOLD="\033[1m"
DIM="\033[2m"
ITAL="\033[3m"
RED="\033[31m"
GREEN="\033[32m"
YELLOW="\033[33m"
BLUE="\033[34m"
MAGENTA="\033[35m"
CYAN="\033[36m"
GREY="\033[90m"
RESET="\033[0m"

# Box-drawing
BOX_TL="┌"
BOX_TR="┐"
BOX_BL="└"
BOX_BR="┘"
BOX_H="─"
BOX_V="│"

# ── headers ────────────────────────────────────────────────────────────────────

axis() {
    local title="$1"
    echo -e "\n${BOLD}${MAGENTA}── axis: ${title} ${RESET}${MAGENTA}$(printf '%.0s─' $(seq 1 $((60 - ${#title}))))${RESET}\n"
}

bench_header() {
    local name="$1" desc="$2"
    echo -e "${BOLD}${CYAN}▸ ${name}${RESET}  ${DIM}${desc}${RESET}"
}

step() {
    echo -e "    ${DIM}$1${RESET}"
}

ok()   { echo -e "    ${GREEN}✓${RESET} $1"; }
warn() { echo -e "    ${YELLOW}!${RESET} $1"; }
fail() { echo -e "    ${RED}✗${RESET} $1"; }

# ── formatting ─────────────────────────────────────────────────────────────────

fmt_size() {
    local bytes=$1
    if   [ "$bytes" -lt 1024 ];    then echo "${bytes} B"
    elif [ "$bytes" -lt 1048576 ]; then awk "BEGIN { printf \"%.1f KB\", $bytes/1024 }"
    else                                awk "BEGIN { printf \"%.1f MB\", $bytes/1048576 }"
    fi
}

fmt_ms() {
    local us=$1
    if   [ "$us" -lt 1000 ];     then printf "%d µs"  "$us"
    elif [ "$us" -lt 1000000 ];  then awk "BEGIN { printf \"%.2f ms\", $us/1000 }"
    else                              awk "BEGIN { printf \"%.2f s\",  $us/1000000 }"
    fi
}

# Horizontal proportional bar.
# Usage: bar <value> <max> [width]
bar() {
    local v=$1 m=$2 w=${3:-32}
    [ "$m" -le 0 ] && m=1
    local filled=$(( v * w / m ))
    [ "$filled" -lt 1 ] && filled=1
    [ "$filled" -gt "$w" ] && filled="$w"
    local b=""
    for ((i=0; i<filled; i++));  do b+="█"; done
    for ((i=filled; i<w; i++));  do b+="░"; done
    echo "$b"
}

# Print a small status line for a build operation.
# Usage: build_status <lang> <ok|fail|skip> [reason]
build_status() {
    local lang="$1" status="$2" reason="${3:-}"
    case "$status" in
        ok)   printf "    ${GREEN}✓${RESET} %-6s\n" "$lang" ;;
        fail) printf "    ${RED}✗${RESET} %-6s ${DIM}%s${RESET}\n" "$lang" "$reason" ;;
        skip) printf "    ${GREY}-${RESET} %-6s ${DIM}skip%s${RESET}\n" "$lang" "${reason:+: $reason}" ;;
    esac
}
