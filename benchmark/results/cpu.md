| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 934.4 ± 272.3 | 633.8 | 1658.9 | 1.00 |
| `lake (cooperative, quantum=256)` | 6982.4 ± 512.4 | 6028.7 | 9370.1 | 7.47 ± 2.25 |
| `c++ (coroutines)` | 2860.0 ± 422.4 | 2081.7 | 4409.3 | 3.06 ± 1.00 |
| `rust (tokio current_thread)` | 3954.2 ± 485.8 | 3161.5 | 5503.7 | 4.23 ± 1.34 |
| `go (goroutines, GOMAXPROCS=1)` | 2195.0 ± 430.7 | 1552.8 | 7213.9 | 2.35 ± 0.83 |
