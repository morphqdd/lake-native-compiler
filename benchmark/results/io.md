| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 27.2 ± 0.7 | 26.1 | 29.8 | 1.20 ± 0.04 |
| `rust (tokio current_thread)` | 36.7 ± 1.0 | 35.0 | 43.0 | 1.62 ± 0.06 |
| `c++ (coroutines, manual scheduler)` | 22.7 ± 0.5 | 21.7 | 24.2 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 42.4 ± 0.8 | 41.1 | 45.3 | 1.87 ± 0.06 |
