| Command | Mean [µs] | Min [µs] | Max [µs] | Relative |
|:---|---:|---:|---:|---:|
| `lake (cooperative, direct syscalls)` | 295.8 ± 171.6 | 192.6 | 5090.3 | 1.00 |
| `rust (tokio current_thread)` | 1194.8 ± 538.4 | 788.2 | 6247.9 | 4.04 ± 2.97 |
| `c++ (coroutines, manual scheduler)` | 1677.4 ± 638.2 | 1071.2 | 6112.2 | 5.67 ± 3.94 |

## Binary sizes

```diff
+ lake       9.1 KB  █░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░   1.0×
! c++       17.1 KB  █░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░   1.9×
- rust     910.6 KB  ████████████████████████████████████████   100.1×
```

> lake is **100.1×** smaller than rust and **1.9×** smaller than c++
