| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 14.1 ± 0.8 | 12.8 | 17.9 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 119.5 ± 7.4 | 108.6 | 133.6 | 8.49 ± 0.70 |
| `rust (tokio current_thread)` | 33.0 ± 1.1 | 30.8 | 36.0 | 2.34 ± 0.15 |
