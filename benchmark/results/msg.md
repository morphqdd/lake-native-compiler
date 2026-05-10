| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 28.2 ± 1.0 | 26.3 | 33.8 | 9.57 ± 1.41 |
| `c++ (coroutines, manual scheduler)` | 3.0 ± 0.4 | 2.4 | 5.0 | 1.00 |
| `rust (tokio mpsc)` | 42.1 ± 2.9 | 39.4 | 59.7 | 14.28 ± 2.26 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 107.2 ± 1.8 | 104.5 | 112.2 | 36.31 ± 5.23 |
