package main

import (
	"crypto/sha256"
	"os"
	"runtime"
)

// GOMAXPROCS=1 — single OS thread, matches the other bench rows
// (cpu / msg / io / spawn).  Go's stdlib SHA-256 is sequential on
// its block-processing loop anyway, but the runtime would otherwise
// be free to park goroutines and GC work on other cores, which
// muddies the single-thread comparison against C / Rust / Lake.
func main() {
	runtime.GOMAXPROCS(1)
	buf := make([]byte, 1048576)
	h := sha256.Sum256(buf)
	os.Stdout.Write(h[:])
}
