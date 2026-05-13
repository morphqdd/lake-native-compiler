| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.4 ± 0.6 | 22.5 | 26.3 | 1.04 ± 0.04 |
| `rust (tokio current_thread)` | 36.5 ± 0.6 | 35.0 | 38.1 | 1.62 ± 0.04 |
| `c++ (coroutines, manual scheduler)` | 22.6 ± 0.5 | 21.7 | 24.5 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 42.3 ± 0.8 | 41.1 | 44.5 | 1.87 ± 0.05 |
