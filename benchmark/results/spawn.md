| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 5.2 ± 0.5 | 4.5 | 7.1 | 2.47 ± 0.54 |
| `go (goroutines, GOMAXPROCS=1)` | 7.5 ± 0.8 | 6.1 | 14.3 | 3.54 ± 0.77 |
| `rust (tokio current_thread)` | 2.1 ± 0.4 | 1.4 | 5.0 | 1.00 |
