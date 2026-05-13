| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 18.3 ± 9.6 | 11.2 | 60.4 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 120.1 ± 6.8 | 107.1 | 132.2 | 6.57 ± 3.46 |
| `rust (tokio current_thread)` | 33.4 ± 1.1 | 31.4 | 38.4 | 1.83 ± 0.96 |
