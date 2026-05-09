package main

import "fmt"

func ponger(in <-chan int, out chan<- int) {
	for range in {
		out <- 1
	}
}

func main() {
	const rounds = 5
	a := make(chan int, 1)
	b := make(chan int, 1)
	go ponger(a, b)
	for i := 0; i < rounds; i++ {
		a <- 1
		<-b
	}
	close(a)
	fmt.Println("5 rounds completed")
}
