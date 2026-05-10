| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 27.3 ± 1.5 | 25.5 | 36.7 | 1.10 ± 0.08 |
| `rust (tokio current_thread)` | 77.7 ± 1.6 | 76.0 | 83.1 | 3.12 ± 0.17 |
| `c++ (coroutines, manual scheduler)` | 24.9 ± 1.3 | 23.3 | 31.8 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 79.1 ± 2.0 | 76.9 | 88.7 | 3.18 ± 0.18 |
