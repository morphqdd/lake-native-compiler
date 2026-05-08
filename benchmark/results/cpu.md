| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 812.8 ± 253.3 | 608.0 | 2441.5 | 1.00 |
| `lake (cooperative, quantum=256)` | 6164.4 ± 591.7 | 5605.8 | 9154.8 | 7.58 ± 2.47 |
| `c++ (coroutines)` | 2187.7 ± 462.5 | 1817.5 | 5626.3 | 2.69 ± 1.01 |
| `rust (tokio current_thread)` | 5127.6 ± 514.0 | 4511.4 | 7588.2 | 6.31 ± 2.06 |
| `go (goroutines, GOMAXPROCS=1)` | 2401.3 ± 455.9 | 1718.7 | 6320.4 | 2.95 ± 1.08 |
