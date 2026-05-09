package main

import (
	"bufio"
	"fmt"
	"os"
)

func main() {
	r := bufio.NewReader(os.Stdin)
	lines := 0
	for {
		b, err := r.ReadByte()
		if err != nil {
			break
		}
		if b == '\n' {
			lines++
		}
	}
	fmt.Println(lines)
}
