| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `c sequential (baseline)` | 862.5 ± 273.5 | 607.7 | 1960.3 | 1.00 |
| `lake (cooperative, quantum=256)` | 27110.4 ± 1209.2 | 25965.9 | 33826.4 | 31.43 ± 10.06 |
| `c++ (coroutines)` | 2780.2 ± 442.9 | 1923.7 | 4176.8 | 3.22 ± 1.14 |
| `rust (tokio current_thread)` | 5017.3 ± 450.5 | 4507.3 | 6359.8 | 5.82 ± 1.92 |
| `go (goroutines, GOMAXPROCS=1)` | 2240.5 ± 375.2 | 1417.2 | 3856.2 | 2.60 ± 0.93 |
