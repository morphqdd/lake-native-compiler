| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 27.7 ± 0.7 | 26.8 | 31.4 | 1.26 ± 0.04 |
| `rust (tokio current_thread)` | 72.6 ± 0.5 | 71.7 | 73.6 | 3.29 ± 0.08 |
| `c++ (coroutines, manual scheduler)` | 22.0 ± 0.5 | 21.2 | 23.3 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 73.3 ± 0.7 | 71.9 | 74.9 | 3.33 ± 0.08 |
