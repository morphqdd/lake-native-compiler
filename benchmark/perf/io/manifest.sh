NAME="io"
DESC="10 workers × write — async I/O contention"
WARMUP=10
LANGS="lake cpp go rust"
LABELS=(
  "lake:lake (cooperative, direct syscalls)"
  "rust:rust (tokio current_thread)"
  "cpp:c++ (coroutines, manual scheduler)"
  "go:go (goroutines, GOMAXPROCS=1)"
)
