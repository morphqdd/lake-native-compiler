| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 27.7 ± 0.9 | 26.5 | 31.0 | 1.21 ± 0.07 |
| `rust (tokio current_thread)` | 37.4 ± 1.4 | 35.4 | 43.1 | 1.64 ± 0.10 |
| `c++ (coroutines, manual scheduler)` | 22.8 ± 1.0 | 21.9 | 28.3 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 44.0 ± 1.6 | 41.1 | 48.8 | 1.93 ± 0.11 |
