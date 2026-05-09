| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.8 ± 0.5 | 22.9 | 24.9 | 1.07 ± 0.03 |
| `rust (tokio current_thread)` | 72.9 ± 0.8 | 71.5 | 75.3 | 3.29 ± 0.09 |
| `c++ (coroutines, manual scheduler)` | 22.1 ± 0.5 | 21.3 | 25.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 73.7 ± 0.7 | 72.3 | 75.0 | 3.33 ± 0.09 |
