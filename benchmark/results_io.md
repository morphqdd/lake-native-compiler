| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 22.7 ± 0.4 | 22.2 | 24.1 | 1.07 ± 0.03 |
| `rust (tokio current_thread)` | 35.1 ± 0.6 | 34.1 | 36.8 | 1.65 ± 0.04 |
| `c++ (coroutines, manual scheduler)` | 21.3 ± 0.4 | 20.6 | 23.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 43.7 ± 0.9 | 41.3 | 46.1 | 2.05 ± 0.06 |
