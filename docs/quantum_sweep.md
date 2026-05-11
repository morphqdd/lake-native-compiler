# Quantum sweep — empirical data

`LAKE_QUANTUM=<N>` overrides the per-actor reduction budget (default
256).  Each CPS block consumes one reduction; on exhaustion the actor
returns `STOP_LIMIT` and the scheduler rotates.

## Measurement

Test workloads:
- `sha 1 MiB` — pure CPU, single actor hashing 1 MiB of `0x41` bytes.
- `curl idle` — TCP server with no background work; latency of a
  single localhost request.
- `curl loaded` — same server + background SHA-256 actor running in a
  tight loop; latency of three sequential curl requests.

Results post-Phase 2a inline (#78), commit 4295345:

```
quantum   sha 1 MiB    curl idle    curl loaded (3 reqs)
   4         9.82 s        8 ms        25 ms        ← +60% CPU overhead
  16         6.85 s        9 ms        26 ms        ← +11%
  64         6.42 s        8 ms        25 ms        ← +4%
 256         6.15 s        8 ms        25 ms        ← current default
1024         6.11 s        8 ms        25 ms        ← -1% CPU vs default
4096         6.08 s        9 ms        26 ms
16384        6.09 s        8 ms       5.92 s        ← latency cliff
65536        6.08 s        7 ms       5.94 s        ← unusable for I/O+CPU
```

## Three regimes

1. **quantum < 64** — per-tick dispatch dominates.  quantum=4 burns
   60% of CPU on context-switching.

2. **64 ≤ quantum ≤ 4096** — sweet spot.  Throughput within 5% of
   peak, latency identical to default.

3. **quantum ≥ 16384** — latency cliff.  Background actor consumes
   tens of thousands of reductions before yielding, I/O actors stall
   for seconds.

## Fairness model

Single-actor preempt window = `quantum × reduction_time`.  Reductions
take ~6 μs each (1 MiB sha256 / ~1M reductions).

```
quantum     1-actor preempt    @ 100 concurrent    @ 1000 concurrent
  256          1.5 ms              150 ms              1.5 s
 1024          6   ms              600 ms              6   s
 4096         25   ms              2.5 s              25   s
16384        100   ms             10   s             100   s
```

A web server with 100 concurrent connections needs quantum ≤ 1024 to
keep p99 under a second.  Game loops at 60fps (16ms ticks) need
quantum ≤ 256 to avoid frame drops.

## Default rationale

256 stays default.  Marginal CPU loss (~1%) vs 1024 buys headroom
for concurrent workloads.  Lake's selling point is massive
concurrency on a single thread — tuning the default for single-actor
microbenchmarks would sacrifice that.

Users who know their workload is single-actor CPU-bound can set
`LAKE_QUANTUM=4096` (or use `lakec -O speed` if/when it folds the
env into the build).

## Future: adaptive quantum (#84-pending)

Per-actor quantum tuned by recent yield patterns.  CPU-bound actor
(yields only on STOP_LIMIT) gets larger quantum; I/O-bound actor
(yields on wait/park) keeps small quantum.  No single value sacrifices
either workload.
