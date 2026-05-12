| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, mailbox)` | 48.7 ± 1.7 | 46.1 | 53.8 | 15.44 ± 1.96 |
| `c++ (coroutines, manual scheduler)` | 3.2 ± 0.4 | 2.4 | 4.5 | 1.00 |
| `rust (tokio mpsc)` | 27.9 ± 1.2 | 26.1 | 32.4 | 8.86 ± 1.14 |
| `go (goroutines, channels, GOMAXPROCS=1)` | 42.2 ± 2.7 | 38.8 | 52.7 | 13.39 ± 1.83 |
