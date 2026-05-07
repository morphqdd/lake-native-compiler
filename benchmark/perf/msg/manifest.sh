NAME="msg"
DESC="ping-pong 100k round-trips — message passing throughput"
WARMUP=10
LANGS="lake cpp go rust"
LABELS=(
  "lake:lake (cooperative, mailbox)"
  "cpp:c++ (coroutines, manual scheduler)"
  "rust:rust (tokio mpsc)"
  "go:go (goroutines, channels, GOMAXPROCS=1)"
)
