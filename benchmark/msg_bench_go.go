package main

// Message-passing benchmark: ping-pong, 100000 round-trips.
// GOMAXPROCS=1 — single OS thread, cooperative scheduling (matches Lake model).

import (
	"fmt"
	"runtime"
	"sync"
)

const N = 100000

func ponger(in <-chan int, out chan<- int, wg *sync.WaitGroup) {
	defer wg.Done()
	for i := 0; i < N; i++ {
		v := <-in
		out <- v
	}
}

func pinger(in <-chan int, out chan<- int, wg *sync.WaitGroup) {
	defer wg.Done()
	for i := 0; i < N; i++ {
		out <- 1
		<-in
	}
}

func main() {
	runtime.GOMAXPROCS(1)
	var wg sync.WaitGroup

	pingToPong := make(chan int, 1)
	pongToPing := make(chan int, 1)

	wg.Add(2)
	go ponger(pingToPong, pongToPing, &wg)
	go pinger(pongToPing, pingToPong, &wg)
	wg.Wait()
	fmt.Print(".\n")
}
