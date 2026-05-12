| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 43.8 ± 1.7 | 42.2 | 51.1 | 1.34 ± 0.07 |
| `go (goroutines, GOMAXPROCS=1)` | 121.7 ± 12.3 | 109.8 | 166.3 | 3.73 ± 0.39 |
| `rust (tokio current_thread)` | 32.6 ± 1.0 | 30.2 | 34.9 | 1.00 |
