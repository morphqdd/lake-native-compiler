| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 27.0 ± 0.8 | 26.0 | 31.0 | 1.21 ± 0.05 |
| `rust (tokio current_thread)` | 36.4 ± 1.0 | 35.1 | 41.4 | 1.62 ± 0.07 |
| `c++ (coroutines, manual scheduler)` | 22.4 ± 0.7 | 21.5 | 26.6 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 42.8 ± 1.2 | 40.8 | 47.5 | 1.91 ± 0.08 |
