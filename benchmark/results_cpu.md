| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 728.6 ± 221.7 | 560.1 | 1632.0 | 1.00 |
| `lake (cooperative, quantum=256)` | 26653.6 ± 505.2 | 25461.3 | 27588.1 | 36.58 ± 11.15 |
| `c++ (coroutines)` | 2281.7 ± 529.0 | 1778.2 | 4540.3 | 3.13 ± 1.20 |
| `rust (tokio current_thread)` | 3356.2 ± 416.5 | 2944.5 | 5119.8 | 4.61 ± 1.51 |
| `go (goroutines, GOMAXPROCS=1)` | 2135.6 ± 384.8 | 1442.9 | 3426.2 | 2.93 ± 1.04 |
