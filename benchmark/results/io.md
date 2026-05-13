| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.5 ± 0.9 | 22.5 | 27.8 | 1.03 ± 0.05 |
| `rust (tokio current_thread)` | 36.7 ± 1.4 | 35.3 | 42.5 | 1.61 ± 0.09 |
| `c++ (coroutines, manual scheduler)` | 22.8 ± 0.8 | 21.8 | 25.8 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 43.1 ± 1.7 | 41.4 | 49.0 | 1.89 ± 0.10 |
