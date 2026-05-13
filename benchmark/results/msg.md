| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 20.3 ± 1.6 | 18.7 | 26.9 | 7.08 ± 1.24 |
| `c++ (coroutines, manual scheduler)` | 2.9 ± 0.4 | 2.2 | 4.3 | 1.00 |
| `rust (tokio mpsc)` | 27.0 ± 1.5 | 25.4 | 32.4 | 9.41 ± 1.55 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 40.0 ± 1.8 | 37.6 | 46.5 | 13.94 ± 2.26 |
