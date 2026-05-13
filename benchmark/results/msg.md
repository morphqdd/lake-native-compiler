| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 28.1 ± 14.0 | 19.1 | 93.6 | 7.73 ± 5.00 |
| `c++ (coroutines, manual scheduler)` | 3.6 ± 1.5 | 2.3 | 18.9 | 1.00 |
| `rust (tokio mpsc)` | 27.9 ± 1.8 | 25.8 | 34.1 | 7.68 ± 3.20 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 41.1 ± 2.2 | 38.0 | 49.1 | 11.33 ± 4.71 |
