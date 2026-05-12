| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 24.5 ± 0.8 | 23.2 | 27.2 | 8.58 ± 1.59 |
| `c++ (coroutines, manual scheduler)` | 2.9 ± 0.5 | 2.2 | 5.2 | 1.00 |
| `rust (tokio mpsc)` | 26.2 ± 1.1 | 25.0 | 31.0 | 9.16 ± 1.72 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 39.2 ± 1.1 | 37.1 | 41.7 | 13.72 ± 2.54 |
