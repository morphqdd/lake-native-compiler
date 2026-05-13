| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 810.2 ± 219.5 | 611.3 | 2203.7 | 1.00 |
| `lake (cooperative, quantum=256)` | 1144.8 ± 892.4 | 724.8 | 11981.6 | 1.41 ± 1.17 |
| `c++ (coroutines)` | 2514.3 ± 418.1 | 1922.7 | 4011.9 | 3.10 ± 0.99 |
| `rust (tokio current_thread)` | 3765.4 ± 423.0 | 3105.5 | 5316.0 | 4.65 ± 1.36 |
| `go (goroutines, GOMAXPROCS=1)` | 2048.5 ± 290.1 | 1548.8 | 3284.2 | 2.53 ± 0.77 |
