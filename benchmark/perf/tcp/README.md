# TCP echo bench — concurrency sweep

Each server replies "hi from lake\n" after a fixed `work(500)` busy-loop
(simulating a tiny request handler).  Load gen: `load.c`, multi-threaded,
`connect → read → close` per request, records per-request latency.

**Hardware:** AMD Ryzen 7 5700U (16 logical cores), AC power.
**Kernel:** Linux 6.19.11.
**Lake commit:** `51fe6d1` (combined submit + wait).
**Total per concurrency level:** 10 000 requests.

## Throughput (rps)

| Concurrency | Lake | C-sync | Rust + tokio | Go |
|---:|---:|---:|---:|---:|
|    1 | **11 317** | 11 203 | 10 336 | 9 795 |
|    4 |    17 364 | 16 959 | **18 313** | 18 755 |
|   16 |    15 987 | **17 567** | 16 244 | 14 348 |
|   64 |    18 310 | **23 713** | 18 201 | 16 707 |
|  256 |     8 300 ⚠ | **25 224** | 21 340 | 21 183 |
| 1024 | **timeout** | **27 538** | timeout | 17 644 |

## p99 latency

| Concurrency | Lake | C-sync | Rust + tokio | Go |
|---:|---:|---:|---:|---:|
|    1 |    145 µs | **122 µs** |   165 µs |   166 µs |
|    4 |    1.7 ms |    3.3 ms | 1.0 ms | **605 µs** |
|   16 |    9.2 ms |    9.7 ms |  9.7 ms |  10.3 ms |
|   64 |   20.0 ms | **12.3 ms** | 18.6 ms |  15.1 ms |
|  256 |  **1005 ms** ⚠ | **24.8 ms** | 41.4 ms |  37.5 ms |
| 1024 |   timeout | **59.4 ms** | timeout |  93.2 ms |

## Plots

- `bench_rps.png` — throughput vs concurrency (log-log)
- `bench_p99.png` — p99 latency vs concurrency
- `bench_tail.png` — p50 vs p99 spread (tail amplification)

## Reading

* **c = 1–16:** all four servers within statistical noise.  Lake leads
  at c = 1 (11 317 rps) and matches everyone else through c = 16.
* **c = 64:** Lake is competitive (18 k rps) but C sync edges ahead at
  24 k.  Tail latency starts growing for everyone (p99 12–20 ms).
* **c = 256:** Lake **catastrophically degrades** — throughput drops to
  8 k rps and p99 latency explodes to **1 second**.  p50 stays at
  4.5 ms — most requests still fast, but a heavy tail.
* **c = 1024:** Lake and tokio both time out (60 s deadline).  C sync
  and Go survive — C sync at 27 k rps with p99 = 59 ms.

## Lake's c=256 cliff — root cause

The wake path uses `emit_wake_by_user_data`: a **linear scan over
io_parked** to find the actor whose `user_data` matches the CQE.  At
c = 256 the parked-actor list grows to ~256 entries.  Each CQE then
scans up to 256 slots; with 10 000 completions per second that's
~2.5 M list iterations on the hot path, all in user-space.

The scan happens between every accept and send completion, and
serialises with the scheduler loop.  Tail latency reflects the
queue buildup that occurs when the scan can't keep up.

## Fix (not yet implemented)

Replace the O(N) scan with O(1) lookup.  Two options:

1. **Hash table** keyed by proc-ctx fat-ptr.  Standard, modest
   memory overhead.
2. **Intrusive pointer in ProcessCtx** holding the io_parked slot
   index.  On wake, swap-and-pop using the stored index — no scan.
   Smaller footprint, no separate hashmap data structure.

Either should restore Lake to tokio/Go-class behaviour at c = 256+.
File a follow-up task before any further bench claims at high
concurrency.

## What this bench does NOT show

* Lake handles 1 M parked actors (separate density bench, no I/O work
  per actor) — that test passes cleanly.  The c = 256 cliff is
  specifically the **scan cost** in the io_uring wake hot path, not
  general scheduler scaling.
* Multishot accept (kernel auto-resubmit) was attempted but reverted —
  it conflicts with the per-CQE wake assumption (requires a per-actor
  CQE queue to handle the burst).  Tracked separately.

## Reproduce

```bash
# rebuild lake compiler
cargo build --release

# rebuild lake server (adjust path)
target/release/lakec -r benchmark/perf/tcp/lake.lake

# build peers
cc -O2 benchmark/perf/tcp/c-sync.c -o /tmp/tcp_c
cd /tmp && go build -o tcp_go benchmark/perf/tcp/go.go
cd /tmp/tcp_rust && cargo build --release  # main.rs from rust.rs

# build load gen
cc -O2 -lpthread benchmark/perf/tcp/load.c -o /tmp/load

# run sweep
./benchmark/perf/tcp/run.sh
```
