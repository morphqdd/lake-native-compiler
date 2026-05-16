| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 30.5 ± 1.0 | 29.4 | 36.6 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 341.0 ± 14.9 | 316.6 | 368.0 | 11.17 ± 0.60 |
| `rust (tokio current_thread)` | 79.4 ± 2.0 | 76.8 | 86.9 | 2.60 ± 0.10 |
