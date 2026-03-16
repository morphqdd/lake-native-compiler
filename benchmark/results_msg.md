| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 65.9 ± 1.4 | 64.8 | 73.7 | 9.61 ± 0.44 |
| `c++ (coroutines, manual scheduler)` | 6.9 ± 0.3 | 6.3 | 8.2 | 1.00 |
| `rust (tokio mpsc)` | 60.7 ± 2.1 | 59.0 | 67.6 | 8.84 ± 0.47 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 84.5 ± 1.3 | 82.6 | 89.1 | 12.30 ± 0.54 |
