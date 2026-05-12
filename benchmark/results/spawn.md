| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 14.0 ± 0.9 | 12.6 | 20.2 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 130.5 ± 18.6 | 107.5 | 171.8 | 9.32 ± 1.45 |
| `rust (tokio current_thread)` | 34.2 ± 2.9 | 30.5 | 43.9 | 2.44 ± 0.26 |
