| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 777.0 ± 223.8 | 617.3 | 1791.7 | 1.00 |
| `lake (cooperative, quantum=256)` | 6865.9 ± 495.1 | 6243.7 | 8936.5 | 8.84 ± 2.62 |
| `c++ (coroutines)` | 2228.2 ± 420.2 | 1870.5 | 4403.4 | 2.87 ± 0.99 |
| `rust (tokio current_thread)` | 3814.9 ± 491.3 | 3095.6 | 5380.8 | 4.91 ± 1.55 |
| `go (goroutines, GOMAXPROCS=1)` | 2377.2 ± 399.6 | 1624.8 | 3600.5 | 3.06 ± 1.02 |
