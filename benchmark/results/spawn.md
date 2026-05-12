| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 14.3 ± 0.6 | 13.3 | 17.5 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 130.1 ± 5.0 | 123.2 | 142.7 | 9.10 ± 0.50 |
| `rust (tokio current_thread)` | 36.0 ± 2.2 | 32.2 | 45.1 | 2.52 ± 0.19 |
