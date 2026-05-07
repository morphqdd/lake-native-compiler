| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 772.5 ± 243.0 | 585.3 | 1636.6 | 1.00 |
| `lake (cooperative, quantum=256, fused self-call)` | 7033.4 ± 552.8 | 6292.7 | 9121.7 | 9.11 ± 2.95 |
| `c++ (coroutines, quantum=256)` | 2088.2 ± 361.8 | 1796.5 | 4280.4 | 2.70 ± 0.97 |
| `rust (tokio current_thread)` | 3312.6 ± 311.0 | 3004.9 | 5014.2 | 4.29 ± 1.41 |
| `go (goroutines, GOMAXPROCS=1)` | 1846.0 ± 427.6 | 1222.7 | 3210.7 | 2.39 ± 0.93 |
