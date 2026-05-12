| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 25.9 ± 0.4 | 25.2 | 27.1 | 1.17 ± 0.03 |
| `rust (tokio current_thread)` | 35.9 ± 0.8 | 34.8 | 40.3 | 1.62 ± 0.05 |
| `c++ (coroutines, manual scheduler)` | 22.1 ± 0.5 | 21.4 | 23.5 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 42.1 ± 0.9 | 40.4 | 46.1 | 1.91 ± 0.06 |
