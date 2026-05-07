// no-starvation: actor B should print "B" within timeout, even though
// actor A spins on a tight CPU loop without yielding.
//
// Go (1.14+) preempts goroutines via SIGURG every ~10ms, so this PASSES.

package main

import (
	"fmt"
	"runtime"
	"sync"
)

func main() {
	runtime.GOMAXPROCS(1)
	var wg sync.WaitGroup
	wg.Add(1)

	// Actor A: spinner — never yields voluntarily.
	go func() {
		x := 0
		for {
			x++
			_ = x
		}
	}()

	// Actor B: printer — should win scheduler time and print.
	go func() {
		defer wg.Done()
		fmt.Println("B")
	}()

	wg.Wait()
}
