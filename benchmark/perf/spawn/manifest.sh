NAME="spawn"
DESC="100k actor spawn — process creation cost (free-list allocator + death cleanup)"
WARMUP=3
LANGS="lake go rust"
LABELS=(
  "lake:lake (cooperative, ExecCtx alloc)"
  "go:go (goroutines, GOMAXPROCS=1)"
  "rust:rust (tokio current_thread)"
)
