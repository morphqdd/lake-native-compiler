| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 751.3 ± 199.3 | 611.0 | 1708.2 | 1.00 |
| `lake (cooperative, quantum=256)` | 6022.7 ± 433.9 | 5659.8 | 8277.0 | 8.02 ± 2.20 |
| `c++ (coroutines)` | 2031.2 ± 224.0 | 1823.0 | 3695.8 | 2.70 ± 0.78 |
| `rust (tokio current_thread)` | 4746.3 ± 266.3 | 4515.1 | 6110.1 | 6.32 ± 1.71 |
| `go (goroutines, GOMAXPROCS=1)` | 2424.9 ± 383.8 | 1667.6 | 3655.3 | 3.23 ± 1.00 |
