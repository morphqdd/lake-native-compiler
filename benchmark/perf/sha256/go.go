package main

import (
	"crypto/sha256"
	"os"
)

func main() {
	buf := make([]byte, 65536)
	h := sha256.Sum256(buf)
	os.Stdout.Write(h[:])
}
