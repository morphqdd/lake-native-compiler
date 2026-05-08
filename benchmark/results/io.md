| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 22.8 ± 0.1 | 22.6 | 23.6 | 1.07 ± 0.02 |
| `rust (tokio current_thread)` | 72.3 ± 0.5 | 71.3 | 73.6 | 3.39 ± 0.05 |
| `c++ (coroutines, manual scheduler)` | 21.3 ± 0.3 | 20.9 | 22.7 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 72.8 ± 0.8 | 71.7 | 75.0 | 3.42 ± 0.06 |
