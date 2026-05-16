| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 1.9 ± 0.2 | 1.6 | 2.8 | 1.00 |
| `lake (cooperative, quantum=256)` | 2.2 ± 0.3 | 1.8 | 4.2 | 1.14 ± 0.20 |
| `c++ (coroutines)` | 5.2 ± 0.4 | 4.6 | 7.1 | 2.76 ± 0.41 |
| `rust (tokio current_thread)` | 10.3 ± 0.4 | 9.6 | 11.5 | 5.44 ± 0.70 |
| `go (goroutines, GOMAXPROCS=1)` | 4.8 ± 0.3 | 4.1 | 8.3 | 2.50 ± 0.35 |
