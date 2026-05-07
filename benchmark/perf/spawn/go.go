// Spawn 100k goroutines that exit immediately.
// Wait for all to complete before exiting.

package main

import (
	"fmt"
	"runtime"
	"sync"
)

func main() {
	runtime.GOMAXPROCS(1)
	const N = 2_000
	var wg sync.WaitGroup
	wg.Add(N)
	for i := 0; i < N; i++ {
		go func() {
			wg.Done()
		}()
	}
	wg.Wait()
	fmt.Println("done")
}
