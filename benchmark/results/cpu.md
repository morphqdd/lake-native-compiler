| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 819.6 ± 203.1 | 624.2 | 2968.1 | 1.00 |
| `lake (cooperative, quantum=256)` | 6764.6 ± 477.6 | 5950.7 | 9435.9 | 8.25 ± 2.13 |
| `c++ (coroutines)` | 2636.8 ± 385.1 | 1972.0 | 4116.5 | 3.22 ± 0.93 |
| `rust (tokio current_thread)` | 3913.1 ± 431.2 | 3211.1 | 5756.3 | 4.77 ± 1.29 |
| `go (goroutines, GOMAXPROCS=1)` | 2220.2 ± 317.9 | 1660.1 | 3815.1 | 2.71 ± 0.78 |
