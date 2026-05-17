| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 25.9 ± 0.5 | 25.1 | 28.0 | 10.28 ± 1.86 |
| `c++ (coroutines, manual scheduler)` | 2.5 ± 0.5 | 2.1 | 4.4 | 1.00 |
| `rust (tokio mpsc)` | 37.4 ± 0.6 | 36.6 | 39.3 | 14.87 ± 2.68 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 100.1 ± 0.8 | 99.0 | 101.6 | 39.78 ± 7.16 |
