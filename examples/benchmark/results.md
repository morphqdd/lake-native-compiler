| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 289.2 ± 89.7 | 188.1 | 760.3 | 1.00 |
| `rust (tokio current_thread)` | 1138.7 ± 329.5 | 785.3 | 2288.8 | 3.94 ± 1.67 |
| `c++ (coroutines, manual scheduler)` | 1727.4 ± 408.8 | 1103.7 | 2888.3 | 5.97 ± 2.33 |
