| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 12.1 ± 0.9 | 11.0 | 17.9 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 117.9 ± 6.5 | 107.6 | 129.6 | 9.72 ± 0.92 |
| `rust (tokio current_thread)` | 32.9 ± 1.6 | 30.0 | 38.3 | 2.71 ± 0.25 |
