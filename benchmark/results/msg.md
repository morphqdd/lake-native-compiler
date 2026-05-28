| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 27.4 ± 0.8 | 26.2 | 31.3 | 10.82 ± 1.75 |
| `c++ (coroutines, manual scheduler)` | 2.5 ± 0.4 | 2.2 | 4.2 | 1.00 |
| `rust (tokio mpsc)` | 27.2 ± 0.5 | 26.6 | 29.7 | 10.73 ± 1.72 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 39.2 ± 0.8 | 37.6 | 41.1 | 15.46 ± 2.48 |
