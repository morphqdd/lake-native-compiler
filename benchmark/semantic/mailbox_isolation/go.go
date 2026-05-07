// mailbox-isolation: each goroutine gets its own channel.  Flooding ch_a
// with 50 messages and processing them must not delay the single send to ch_b.

package main

import (
	"fmt"
	"runtime"
	"sync"
)

func main() {
	runtime.GOMAXPROCS(1)
	ch_a := make(chan int, 64)
	ch_b := make(chan int, 1)

	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		for i := 0; i < 50; i++ {
			<-ch_a
			fmt.Print("A")
		}
	}()
	go func() {
		defer wg.Done()
		<-ch_b
		fmt.Print("B")
	}()

	for i := 0; i < 50; i++ {
		ch_a <- 1
	}
	ch_b <- 1
	wg.Wait()
}
