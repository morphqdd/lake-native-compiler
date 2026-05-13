NAME="sha256"
DESC="SHA-256 of 64 KiB zeroed buffer — single-thread, byte-validated"
WARMUP=5
LANGS="seq lake cpp rust go"
LABELS=(
  "seq:c sequential (self-contained, no libcrypto)"
  "lake:lake (std.crypto.sha256, pure-Lake)"
  "cpp:c++ (self-contained)"
  "rust:rust (sha2 crate)"
  "go:go (crypto/sha256)"
)
