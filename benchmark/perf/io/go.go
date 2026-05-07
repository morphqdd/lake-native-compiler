package main

import (
	"os"
	"runtime"
	"sync"
)

func worker(n int, wg *sync.WaitGroup) {
	defer wg.Done()
	for i := 0; i < n; i++ {
		os.Stdout.Write([]byte("hello\n"))
		runtime.Gosched()
	}
}

func main() {
	runtime.GOMAXPROCS(1)
	var wg sync.WaitGroup
	for i := 0; i < 10; i++ {
		wg.Add(1)
		go worker(10000, &wg)
	}
	wg.Wait()
}
