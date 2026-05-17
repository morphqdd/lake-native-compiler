| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 758.3 ± 210.8 | 596.7 | 1676.1 | 1.00 |
| `lake (cooperative, quantum=256)` | 1212.9 ± 1037.6 | 719.3 | 12084.3 | 1.60 ± 1.44 |
| `c++ (coroutines)` | 2238.2 ± 453.7 | 1865.3 | 4375.4 | 2.95 ± 1.02 |
| `rust (tokio current_thread)` | 4917.6 ± 416.5 | 4500.7 | 6457.4 | 6.49 ± 1.88 |
| `go (goroutines, GOMAXPROCS=1)` | 2443.5 ± 404.3 | 1643.1 | 3696.6 | 3.22 ± 1.04 |
