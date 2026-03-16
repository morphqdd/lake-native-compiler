| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 69.7 ± 1.6 | 68.6 | 77.4 | 1.35 ± 0.03 |
| `rust (tokio current_thread)` | 84.7 ± 2.0 | 83.0 | 92.3 | 1.64 ± 0.04 |
| `c++ (coroutines, manual scheduler)` | 51.5 ± 0.4 | 50.9 | 53.0 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 104.4 ± 0.9 | 102.5 | 106.0 | 2.03 ± 0.02 |
