| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 812.3 ± 252.7 | 609.6 | 2019.4 | 1.00 |
| `lake (cooperative, quantum=256)` | 6844.9 ± 576.4 | 5813.4 | 9983.0 | 8.43 ± 2.72 |
| `c++ (coroutines)` | 2899.7 ± 461.9 | 2014.3 | 4588.6 | 3.57 ± 1.25 |
| `rust (tokio current_thread)` | 3962.5 ± 451.1 | 3146.5 | 5583.3 | 4.88 ± 1.62 |
| `go (goroutines, GOMAXPROCS=1)` | 2201.7 ± 369.6 | 1523.7 | 4166.7 | 2.71 ± 0.96 |
