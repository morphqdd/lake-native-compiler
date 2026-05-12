| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 40.4 ± 1.0 | 39.1 | 45.2 | 15.11 ± 2.80 |
| `c++ (coroutines, manual scheduler)` | 2.7 ± 0.5 | 2.1 | 4.5 | 1.00 |
| `rust (tokio mpsc)` | 25.9 ± 0.6 | 24.7 | 27.8 | 9.70 ± 1.79 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 39.3 ± 1.0 | 37.5 | 43.1 | 14.69 ± 2.73 |
