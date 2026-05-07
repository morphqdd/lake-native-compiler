| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 22.4 ± 0.7 | 21.5 | 26.4 | 8.30 ± 1.57 |
| `c++ (coroutines, manual scheduler)` | 2.7 ± 0.5 | 2.1 | 4.8 | 1.00 |
| `rust (tokio mpsc)` | 37.7 ± 0.4 | 37.1 | 38.8 | 13.99 ± 2.61 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 101.1 ± 2.1 | 98.8 | 107.3 | 37.52 ± 7.04 |
