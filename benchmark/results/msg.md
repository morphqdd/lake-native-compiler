| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 22.9 ± 0.6 | 22.1 | 25.1 | 9.16 ± 1.76 |
| `c++ (coroutines, manual scheduler)` | 2.5 ± 0.5 | 2.1 | 5.6 | 1.00 |
| `rust (tokio mpsc)` | 37.8 ± 0.8 | 36.9 | 40.4 | 15.09 ± 2.89 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 101.2 ± 1.7 | 98.8 | 106.4 | 40.43 ± 7.72 |
