| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 814.0 ± 241.4 | 606.8 | 1933.7 | 1.00 |
| `lake (cooperative, quantum=256)` | 6826.6 ± 554.2 | 6258.7 | 9842.3 | 8.39 ± 2.58 |
| `c++ (coroutines)` | 2407.8 ± 499.8 | 1891.1 | 5934.0 | 2.96 ± 1.07 |
| `rust (tokio current_thread)` | 5094.9 ± 447.3 | 4570.4 | 6650.8 | 6.26 ± 1.94 |
| `go (goroutines, GOMAXPROCS=1)` | 2468.9 ± 405.6 | 1694.7 | 4089.4 | 3.03 ± 1.03 |
