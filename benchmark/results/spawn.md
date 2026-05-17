| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, ExecCtx alloc)` | 42.1 ± 1.9 | 39.2 | 48.5 | 1.00 |
| `go (goroutines, GOMAXPROCS=1)` | 373.8 ± 29.2 | 323.8 | 415.3 | 8.88 ± 0.80 |
| `rust (tokio current_thread)` | 85.1 ± 3.9 | 77.9 | 93.1 | 2.02 ± 0.13 |
