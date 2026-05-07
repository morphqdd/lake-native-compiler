NAME="cpu"
DESC="8 workers × fib(100k) — pure compute, single-thread"
WARMUP=5
LANGS="seq lake cpp go rust"
LABELS=(
  "seq:c sequential (baseline)"
  "lake:lake (cooperative, quantum=256)"
  "cpp:c++ (coroutines)"
  "rust:rust (tokio current_thread)"
  "go:go (goroutines, GOMAXPROCS=1)"
)
