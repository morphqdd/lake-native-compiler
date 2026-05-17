| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 65.9 ± 3.9 | 60.4 | 74.7 | 9.32 ± 1.35 |
| `c++ (coroutines, manual scheduler)` | 7.1 ± 0.9 | 5.3 | 10.7 | 1.00 |
| `rust (tokio mpsc)` | 80.8 ± 2.0 | 78.1 | 87.7 | 11.44 ± 1.54 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 185.7 ± 4.0 | 180.0 | 196.3 | 26.27 ± 3.53 |
