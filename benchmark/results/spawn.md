| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 9.1 ± 0.5 | 8.5 | 10.8 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 203.8 ± 9.1 | 191.7 | 223.5 | 22.45 ± 1.53 |
| `rust (tokio current_thread)` | 35.9 ± 1.5 | 33.9 | 41.5 | 3.96 ± 0.26 |
