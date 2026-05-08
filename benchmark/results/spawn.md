| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 8.3 ± 0.4 | 7.9 | 10.2 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 202.0 ± 15.6 | 189.6 | 253.4 | 24.34 ± 2.25 |
| `rust (tokio current_thread)` | 36.4 ± 0.5 | 35.7 | 38.3 | 4.38 ± 0.23 |
