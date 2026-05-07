| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 19.6 ± 0.5 | 19.1 | 22.6 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 300.5 ± 23.5 | 270.9 | 341.0 | 15.36 ± 1.26 |
| `rust (tokio current_thread)` | 78.9 ± 3.3 | 74.6 | 86.1 | 4.03 ± 0.19 |
