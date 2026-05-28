| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 807.1 ± 234.3 | 614.9 | 1734.5 | 1.00 |
| `lake (cooperative, quantum=256)` | 1455.4 ± 698.1 | 953.6 | 7399.3 | 1.80 ± 1.01 |
| `c++ (coroutines)` | 2487.5 ± 466.3 | 1888.1 | 4276.0 | 3.08 ± 1.06 |
| `rust (tokio current_thread)` | 3691.4 ± 486.6 | 3112.8 | 5252.8 | 4.57 ± 1.46 |
| `go (goroutines, GOMAXPROCS=1)` | 2243.7 ± 375.6 | 1538.9 | 3738.1 | 2.78 ± 0.93 |
