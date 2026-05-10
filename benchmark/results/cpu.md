| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 936.1 ± 270.4 | 671.8 | 1988.5 | 1.00 |
| `lake (cooperative, quantum=256)` | 7250.2 ± 500.0 | 6504.9 | 11097.9 | 7.75 ± 2.30 |
| `c++ (coroutines)` | 2823.1 ± 511.5 | 2080.9 | 5766.3 | 3.02 ± 1.03 |
| `rust (tokio current_thread)` | 5500.0 ± 430.9 | 4851.5 | 6772.4 | 5.88 ± 1.76 |
| `go (goroutines, GOMAXPROCS=1)` | 2484.2 ± 385.2 | 1838.7 | 5894.0 | 2.65 ± 0.87 |
