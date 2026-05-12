| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 913.9 ± 259.2 | 649.2 | 1762.4 | 1.00 |
| `lake (cooperative, quantum=256)` | 7219.7 ± 513.2 | 6468.5 | 9897.5 | 7.90 ± 2.31 |
| `c++ (coroutines)` | 2687.1 ± 438.2 | 1996.4 | 4646.9 | 2.94 ± 0.96 |
| `rust (tokio current_thread)` | 3935.2 ± 428.2 | 3283.0 | 5637.4 | 4.31 ± 1.31 |
| `go (goroutines, GOMAXPROCS=1)` | 2300.8 ± 384.6 | 1559.2 | 4754.8 | 2.52 ± 0.83 |
