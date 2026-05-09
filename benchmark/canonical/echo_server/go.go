package main

import "net"

func main() {
	l, _ := net.Listen("tcp", ":8080")
	for {
		c, err := l.Accept()
		if err != nil {
			continue
		}
		go func(c net.Conn) {
			c.Write([]byte("hi\n"))
			c.Close()
		}(c)
	}
}
