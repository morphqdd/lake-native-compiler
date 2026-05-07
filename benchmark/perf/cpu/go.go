package main

import (
	"os"
	"runtime"
	"sync"
)

// CPU-bound benchmark: 8 goroutines, each computing fib(100000) iteratively.
// GOMAXPROCS=1 — single OS thread, cooperative scheduling (matches Lake model).

func fibIter(n int) int {
	a, b := 0, 1
	for i := 0; i < n; i++ {
		a, b = b, a+b
	}
	return b
}

func worker(wg *sync.WaitGroup) {
	defer wg.Done()
	result := fibIter(100000)
	_ = result
	os.Stdout.WriteString(".\n")
}

func main() {
	runtime.GOMAXPROCS(1)
	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go worker(&wg)
	}
	wg.Wait()
}
