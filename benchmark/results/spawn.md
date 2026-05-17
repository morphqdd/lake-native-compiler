| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 13.8 ± 0.7 | 13.0 | 19.1 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 197.0 ± 7.7 | 185.4 | 211.9 | 14.27 ± 0.90 |
| `rust (tokio current_thread)` | 35.8 ± 0.6 | 34.8 | 38.4 | 2.60 ± 0.13 |
