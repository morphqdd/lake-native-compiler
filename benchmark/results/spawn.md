| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 13.8 ± 0.5 | 12.9 | 15.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 202.4 ± 13.7 | 186.1 | 244.5 | 14.72 ± 1.14 |
| `rust (tokio current_thread)` | 36.0 ± 0.9 | 34.0 | 37.4 | 2.62 ± 0.12 |
