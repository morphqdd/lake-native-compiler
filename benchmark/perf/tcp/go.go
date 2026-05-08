package main
import ("net"; "runtime")
var sink int64
func work(n int64) { for i := int64(0); i < n; i++ { sink += i } }
func main() {
    runtime.GOMAXPROCS(1)
    l, _ := net.Listen("tcp", "127.0.0.1:8082")
    for {
        c, err := l.Accept()
        if err != nil { continue }
        go func(c net.Conn) {
            work(500)
            c.Write([]byte("hi from lake\n"))
            c.Close()
        }(c)
    }
}
