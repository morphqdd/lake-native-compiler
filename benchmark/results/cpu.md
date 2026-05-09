| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 780.3 ± 226.3 | 605.2 | 1735.0 | 1.00 |
| `lake (cooperative, quantum=256)` | 6339.7 ± 538.2 | 5764.4 | 8474.5 | 8.12 ± 2.45 |
| `c++ (coroutines)` | 2299.5 ± 488.4 | 1845.3 | 4973.3 | 2.95 ± 1.06 |
| `rust (tokio current_thread)` | 5108.6 ± 431.8 | 4580.7 | 6499.6 | 6.55 ± 1.98 |
| `go (goroutines, GOMAXPROCS=1)` | 2439.2 ± 394.9 | 1623.2 | 4146.5 | 3.13 ± 1.04 |
