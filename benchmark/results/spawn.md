| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 13.3 ± 0.7 | 12.6 | 17.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 120.0 ± 6.8 | 107.6 | 130.4 | 9.02 ± 0.68 |
| `rust (tokio current_thread)` | 32.8 ± 1.4 | 30.6 | 36.9 | 2.47 ± 0.16 |
