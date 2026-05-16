| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 52.9 ± 1.1 | 51.2 | 58.7 | 8.63 ± 0.60 |
| `c++ (coroutines, manual scheduler)` | 6.1 ± 0.4 | 5.3 | 7.9 | 1.00 |
| `rust (tokio mpsc)` | 78.3 ± 1.1 | 76.7 | 81.8 | 12.78 ± 0.87 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 183.4 ± 1.2 | 181.5 | 185.7 | 29.93 ± 2.00 |
