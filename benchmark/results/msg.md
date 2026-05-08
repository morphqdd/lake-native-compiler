| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 23.1 ± 0.5 | 22.5 | 24.7 | 9.29 ± 1.72 |
| `c++ (coroutines, manual scheduler)` | 2.5 ± 0.5 | 2.1 | 4.5 | 1.00 |
| `rust (tokio mpsc)` | 37.3 ± 0.6 | 36.5 | 39.3 | 15.01 ± 2.77 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 99.7 ± 0.7 | 98.3 | 101.4 | 40.11 ± 7.37 |
