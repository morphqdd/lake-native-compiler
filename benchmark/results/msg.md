| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `c++ (coroutines, manual scheduler)` | 2.4 ± 0.4 | 2.1 | 4.5 | 1.00 |
| `lake (cooperative, mailbox)` | 21.7 ± 0.2 | 21.2 | 22.8 | 9.16 ± 1.46 |
| `rust (tokio mpsc)` | 24.9 ± 0.4 | 24.3 | 26.7 | 10.52 ± 1.67 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 35.5 ± 0.8 | 33.8 | 37.6 | 15.02 ± 2.41 |
