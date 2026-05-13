| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 20.9 ± 1.8 | 19.0 | 27.9 | 7.08 ± 1.16 |
| `c++ (coroutines, manual scheduler)` | 3.0 ± 0.4 | 2.2 | 4.5 | 1.00 |
| `rust (tokio mpsc)` | 27.5 ± 1.5 | 25.3 | 33.3 | 9.32 ± 1.40 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 41.5 ± 2.1 | 38.0 | 47.8 | 14.07 ± 2.10 |
