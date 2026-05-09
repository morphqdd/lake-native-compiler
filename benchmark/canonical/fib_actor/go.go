package main

import "fmt"

func fib(n, a, b int) int {
	if n == 0 {
		return a
	}
	return fib(n-1, b, a+b)
}

func main() {
	fmt.Println(fib(20, 0, 1))
}
