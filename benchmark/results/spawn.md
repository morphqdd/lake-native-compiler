| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 17.5 ± 1.2 | 16.0 | 26.2 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 117.3 ± 4.9 | 107.7 | 128.4 | 6.71 ± 0.55 |
| `rust (tokio current_thread)` | 31.8 ± 1.3 | 30.3 | 41.0 | 1.82 ± 0.15 |
