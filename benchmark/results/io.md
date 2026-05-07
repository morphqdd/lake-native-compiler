| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 23.4 ± 0.6 | 22.5 | 25.2 | 1.07 ± 0.04 |
| `rust (tokio current_thread)` | 72.7 ± 0.7 | 71.4 | 74.6 | 3.31 ± 0.10 |
| `c++ (coroutines, manual scheduler)` | 22.0 ± 0.7 | 21.1 | 25.3 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 73.0 ± 1.0 | 71.7 | 77.7 | 3.32 ± 0.11 |
