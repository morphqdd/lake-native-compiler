| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 902.6 ± 257.4 | 616.0 | 1985.4 | 1.00 |
| `lake (cooperative, quantum=256)` | 1183.8 ± 745.7 | 747.1 | 7058.2 | 1.31 ± 0.91 |
| `c++ (coroutines)` | 2876.2 ± 381.9 | 2130.6 | 4144.9 | 3.19 ± 1.00 |
| `rust (tokio current_thread)` | 4368.3 ± 628.2 | 3440.5 | 7770.6 | 4.84 ± 1.55 |
| `go (goroutines, GOMAXPROCS=1)` | 2051.4 ± 280.5 | 1536.6 | 3684.8 | 2.27 ± 0.72 |
