| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 23.4 ± 0.8 | 22.2 | 26.8 | 7.52 ± 1.18 |
| `c++ (coroutines, manual scheduler)` | 3.1 ± 0.5 | 2.2 | 4.7 | 1.00 |
| `rust (tokio mpsc)` | 26.6 ± 1.0 | 25.4 | 30.6 | 8.55 ± 1.35 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 39.7 ± 0.7 | 38.4 | 41.4 | 12.78 ± 1.97 |
