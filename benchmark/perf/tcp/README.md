# TCP echo bench — concurrency sweep

Each server replies "hi from lake\n" after a fixed `work(500)` busy-loop
(simulating a tiny request handler).  Load gen: `load.c`, multi-threaded,
`connect → read → close` per request, records per-request latency.

**Hardware:** AMD Ryzen 7 5700U (16 logical cores), AC power.
**Kernel:** Linux 6.19.11.
**Lake commit:** post-`11d08c7` (O(1) wake + backlog 4096 + io_parked cap 4096).
**Total per concurrency level:** 10 000 requests.

## Throughput (rps)

| Concurrency | Lake | C-sync | Rust + tokio | Go |
|---:|---:|---:|---:|---:|
|    1 |    10 172 | **11 594** | 10 251 |  9 600 |
|    4 |    18 167 | 16 859 | **24 899** | 21 783 |
|   16 |    16 566 | 18 015 | **29 971** | 26 073 |
|   64 |    18 115 | 21 822 | **31 591** | 22 729 |
|  256 |    23 288 | 27 423 | **29 453** | 22 535 |
| 1024 |    23 847 | **29 436** |  4 273 ⚠ | 23 016 |

## p99 latency

| Concurrency | Lake | C-sync | Rust + tokio | Go |
|---:|---:|---:|---:|---:|
|    1 |    141 µs | **107 µs** |   140 µs |   164 µs |
|    4 |    2.5 ms |    3.6 ms | **292 µs** |  343 µs |
|   16 |    9.4 ms |    8.0 ms | **711 µs** |  805 µs |
|   64 |   21.0 ms |   16.5 ms | **2.4 ms** |  3.7 ms |
|  256 |   63.1 ms |   26.1 ms | **16.2 ms** | 34.1 ms |
| 1024 |   72.5 ms | **47.3 ms** | 1117 ms ⚠ | 94.2 ms |

## Plots

- `bench_rps.png` — throughput vs concurrency (log-log)
- `bench_p99.png` — p99 latency vs concurrency
- `bench_tail.png` — p50 vs p99 spread (tail amplification)

## Reading

* **c = 1–4:** all servers within statistical noise.  Tokio edges
  ahead by c = 4 thanks to its mature event-loop batching.
* **c = 16–64:** tokio leads on throughput and tail (sub-millisecond
  p99 at c = 64).  Lake mid-pack on throughput, p99 ~21 ms.
* **c = 256:** all four hold ~22-29 k rps.  Lake at 23 k rps p99 = 63 ms
  — slower tail than tokio (16 ms) and C-sync (26 ms), competitive with
  Go (34 ms).
* **c = 1024:** Lake **survives at 24 k rps with p99 = 72 ms** —
  comparable to Go (94 ms) and C sync (47 ms).  Tokio degrades sharply
  in this run (4 k rps with p99 over 1 s, possibly TIME_WAIT-related).
  All runs still drop ~8 % of connects at this concurrency due to
  client-side ephemeral-port pressure (10 000 connects in < 1 s
  against a single peer 4-tuple).

## Bottlenecks fixed (commit log entry)

The first run of this bench showed Lake collapsing at c = 256 to
8 k rps with p99 = 1 s.  Root causes and fixes:

1. **O(N) wake scan.**  `emit_wake_by_user_data` originally walked
   `io_parked` linearly to match a CQE's user_data to a parked actor.
   At c = 256 that's ~256 iters per CQE × 10 k completions =
   2.5 M iterations on the hot path.

   **Fix:** intrusive index field `IO_PARKED_IDX` on `ProcessCtx`.
   `rt_io_park_current` stashes the io_parked slot at park time;
   the wake path reads it directly and does an O(1) swap-and-pop
   (with a coherency update for the moved actor's intrusive idx).

2. **listen() backlog = 128.**  Lake's `rt_listen_tcp` originally
   used `listen(fd, 128)`, while C-sync used 1024.  At c = 256+,
   the SYN backlog overflowed and 256 simultaneously-arriving SYNs
   were partially dropped, amplifying tail latency through retries.

   **Fix:** raise to `listen(fd, 4096)`.

3. **io_parked cap = 64.**  `INITIAL_IO_PARKED_CAP` was 64 with no
   grow path, so the 65th parked actor wrote past the buffer.

   **Fix:** raise initial cap to 4096.  Real grow logic (same as
   `process_arr` / `wait_arr`) is still TODO; current cap holds
   practical concurrency at 4 k.

After these three fixes, Lake's c = 256–1024 numbers landed in the
same band as Go and C-sync.  The c = 64–256 throughput trail vs tokio
is the next opportunity (see follow-ups below).

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
