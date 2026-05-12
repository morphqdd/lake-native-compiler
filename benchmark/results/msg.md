| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 24.7 ± 0.7 | 23.6 | 27.9 | 9.38 ± 1.57 |
| `c++ (coroutines, manual scheduler)` | 2.6 ± 0.4 | 2.2 | 4.6 | 1.00 |
| `rust (tokio mpsc)` | 26.2 ± 0.9 | 25.1 | 30.4 | 9.93 ± 1.67 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 39.7 ± 1.2 | 38.0 | 46.1 | 15.08 ± 2.53 |
