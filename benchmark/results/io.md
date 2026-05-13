| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.8 ± 0.8 | 22.6 | 27.6 | 1.05 ± 0.05 |
| `rust (tokio current_thread)` | 38.1 ± 2.3 | 35.4 | 46.8 | 1.68 ± 0.12 |
| `c++ (coroutines, manual scheduler)` | 22.7 ± 0.7 | 21.7 | 26.2 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 43.5 ± 2.3 | 41.5 | 52.8 | 1.91 ± 0.12 |
