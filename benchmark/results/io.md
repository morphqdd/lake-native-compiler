| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 54.6 ± 0.7 | 53.6 | 58.0 | 1.04 ± 0.02 |
| `rust (tokio current_thread)` | 138.3 ± 1.2 | 136.7 | 141.2 | 2.64 ± 0.04 |
| `c++ (coroutines, manual scheduler)` | 52.5 ± 0.7 | 51.5 | 55.4 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 145.0 ± 1.7 | 143.3 | 150.9 | 2.76 ± 0.05 |
