| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.4 ± 0.5 | 22.9 | 25.5 | 1.06 ± 0.04 |
| `rust (tokio current_thread)` | 73.4 ± 0.8 | 71.7 | 75.5 | 3.31 ± 0.10 |
| `c++ (coroutines, manual scheduler)` | 22.2 ± 0.6 | 21.4 | 25.5 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 73.8 ± 1.0 | 72.4 | 76.7 | 3.33 ± 0.11 |
