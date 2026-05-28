| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 28.4 ± 0.5 | 27.7 | 30.2 | 1.27 ± 0.04 |
| `rust (tokio current_thread)` | 36.4 ± 1.2 | 34.8 | 45.1 | 1.63 ± 0.06 |
| `c++ (coroutines, manual scheduler)` | 22.3 ± 0.5 | 21.6 | 24.9 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 42.7 ± 0.9 | 41.2 | 46.5 | 1.91 ± 0.06 |
