| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 894.9 ± 279.6 | 597.1 | 1713.0 | 1.00 |
| `lake (cooperative, quantum=256)` | 6482.4 ± 520.8 | 5735.8 | 8084.5 | 7.24 ± 2.34 |
| `c++ (coroutines)` | 2939.0 ± 424.4 | 2008.5 | 4328.1 | 3.28 ± 1.13 |
| `rust (tokio current_thread)` | 4096.5 ± 398.4 | 3266.6 | 5303.8 | 4.58 ± 1.50 |
| `go (goroutines, GOMAXPROCS=1)` | 2257.1 ± 354.6 | 1566.8 | 3549.0 | 2.52 ± 0.88 |
