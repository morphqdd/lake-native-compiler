| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 24.8 ± 1.3 | 23.0 | 32.5 | 1.09 ± 0.07 |
| `rust (tokio current_thread)` | 36.7 ± 0.7 | 35.1 | 38.3 | 1.61 ± 0.07 |
| `c++ (coroutines, manual scheduler)` | 22.8 ± 0.9 | 21.7 | 28.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 43.5 ± 1.9 | 41.6 | 53.6 | 1.91 ± 0.11 |
