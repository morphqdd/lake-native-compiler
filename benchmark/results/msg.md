| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 24.5 ± 0.6 | 23.3 | 25.9 | 9.17 ± 1.78 |
| `c++ (coroutines, manual scheduler)` | 2.7 ± 0.5 | 2.1 | 4.3 | 1.00 |
| `rust (tokio mpsc)` | 37.7 ± 0.5 | 36.8 | 39.3 | 14.10 ± 2.72 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 100.2 ± 1.3 | 98.3 | 103.6 | 37.50 ± 7.24 |
