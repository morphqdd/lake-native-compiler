| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 888.2 ± 276.6 | 626.9 | 1777.7 | 1.00 |
| `lake (cooperative, quantum=256)` | 6492.2 ± 504.4 | 5709.5 | 9546.2 | 7.31 ± 2.35 |
| `c++ (coroutines)` | 2570.4 ± 516.2 | 1872.6 | 4281.1 | 2.89 ± 1.07 |
| `rust (tokio current_thread)` | 5297.0 ± 468.4 | 4625.3 | 6379.5 | 5.96 ± 1.93 |
| `go (goroutines, GOMAXPROCS=1)` | 2363.0 ± 375.9 | 1686.0 | 3694.8 | 2.66 ± 0.93 |
