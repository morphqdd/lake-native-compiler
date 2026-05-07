// fairness: 4 goroutines each count down M iterations and print "done".
// Go's scheduler preempts every ~10ms (Go 1.14+) so all 4 progress.

package main

import (
	"fmt"
	"runtime"
	"sync"
)

const N = 4
const M = 2_000_000

func main() {
	runtime.GOMAXPROCS(1)
	var wg sync.WaitGroup
	wg.Add(N)
	for i := 0; i < N; i++ {
		go func() {
			defer wg.Done()
			x := M
			for x > 0 {
				x--
			}
			fmt.Println("done")
		}()
	}
	wg.Wait()
}
