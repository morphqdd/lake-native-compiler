| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 9.5 ± 0.5 | 8.7 | 11.3 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 193.3 ± 7.1 | 182.4 | 203.6 | 20.29 ± 1.38 |
| `rust (tokio current_thread)` | 35.9 ± 1.2 | 33.7 | 38.6 | 3.77 ± 0.25 |
