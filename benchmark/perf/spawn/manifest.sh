NAME="spawn"
DESC="2k actor spawn — process creation cost (Lake static queue caps burst at 256 slots)"
WARMUP=3
LANGS="lake go rust"
LABELS=(
  "lake:lake (cooperative, ExecCtx alloc)"
  "go:go (goroutines, GOMAXPROCS=1)"
  "rust:rust (tokio current_thread)"
)
