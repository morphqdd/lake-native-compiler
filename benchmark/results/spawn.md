| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 44.3 ± 1.1 | 42.6 | 47.5 | 1.39 ± 0.05 |
| `go (goroutines, GOMAXPROCS=1)` | 117.4 ± 3.2 | 109.1 | 120.5 | 3.69 ± 0.13 |
| `rust (tokio current_thread)` | 31.8 ± 0.7 | 30.7 | 34.2 | 1.00 |
