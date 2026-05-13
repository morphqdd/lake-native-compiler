| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 924.3 ± 231.5 | 652.6 | 1855.9 | 1.00 |
| `lake (cooperative, quantum=256)` | 1160.6 ± 765.7 | 777.1 | 6997.5 | 1.26 ± 0.89 |
| `c++ (coroutines)` | 2781.5 ± 413.4 | 2072.1 | 5300.7 | 3.01 ± 0.88 |
| `rust (tokio current_thread)` | 3871.4 ± 355.8 | 3293.9 | 5597.9 | 4.19 ± 1.12 |
| `go (goroutines, GOMAXPROCS=1)` | 2054.8 ± 225.9 | 1583.2 | 3352.2 | 2.22 ± 0.61 |
