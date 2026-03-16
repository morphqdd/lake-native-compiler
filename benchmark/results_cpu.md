| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 2.1 ± 0.1 | 1.9 | 2.6 | 1.00 |
| `lake (cooperative, quantum=256)` | 19.9 ± 0.6 | 19.3 | 25.2 | 9.51 ± 0.62 |
| `c++ (coroutines)` | 6.1 ± 0.3 | 5.5 | 7.6 | 2.90 ± 0.21 |
| `rust (tokio current_thread)` | 9.7 ± 0.2 | 9.3 | 10.5 | 4.65 ± 0.28 |
| `go (goroutines, GOMAXPROCS=1)` | 4.6 ± 0.2 | 4.0 | 5.3 | 2.19 ± 0.16 |
