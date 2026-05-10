| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 13.3 ± 0.7 | 12.1 | 15.9 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 204.1 ± 11.3 | 190.9 | 225.6 | 15.32 ± 1.16 |
| `rust (tokio current_thread)` | 40.9 ± 1.5 | 38.3 | 45.4 | 3.07 ± 0.20 |
