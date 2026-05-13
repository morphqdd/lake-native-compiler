| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.5 ± 0.9 | 22.4 | 27.1 | 1.04 ± 0.05 |
| `rust (tokio current_thread)` | 37.1 ± 1.2 | 35.5 | 43.6 | 1.64 ± 0.07 |
| `c++ (coroutines, manual scheduler)` | 22.6 ± 0.7 | 21.8 | 25.7 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 44.1 ± 3.0 | 41.2 | 56.5 | 1.95 ± 0.15 |
