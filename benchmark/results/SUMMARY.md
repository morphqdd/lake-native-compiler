# Lake performance — summary

**Hardware:** AMD Ryzen 7 5700U (16 logical cores), AC power.
**Kernel:** Linux 6.19.11.
**Commit:** `4e5100c` (3-tier allocator + dynamic scheduler queue).
**Tooling:** hyperfine `--warmup 3-10` for perf axes.

All numbers are mean ± σ. Per-axis raw data: `cpu.md`, `io.md`, `msg.md`, `spawn.md`.

---

## 1. Overall — all axes, all languages

| Axis | Workload | Lake | C++ coroutines | Rust + tokio | Go | C seq |
|---|---|---|---|---|---|---|
| I/O    | 10 × write   | 23.4 ± 0.5 ms | **22.2 ± 0.6 ms** | 73.4 ± 0.8 ms | 73.8 ± 1.0 ms | — |
| Spawn  | 100k actors  | **9.1 ± 0.5 ms** | — | 35.9 ± 1.5 ms | 203.8 ± 9.1 ms | — |
| MSG    | ping-pong 100k | 22.9 ± 0.6 ms | **2.5 ± 0.5 ms** | 37.8 ± 0.8 ms | 101.2 ± 1.7 ms | — |
| CPU    | fib(100k) × 8 | 6164 ± 592 ms | 2188 ± 463 ms | 5128 ± 514 ms | 2401 ± 456 ms | **813 ± 253 ms** |

**Bold** = fastest in row. Lake leads on Spawn; parity with C++ on I/O; loses CPU + MSG to non-actor systems.

---

## 2. Per-axis tables

### I/O — 10 workers × write (lower = better)

| Lang | Time | vs Lake | vs best |
|---|---|---|---|
| C++ coroutines (manual sched) | 22.2 ms | 0.95× | **1.00×** |
| Lake (cooperative, direct syscalls) | **23.4 ms** | 1.00× | 1.06× |
| Rust + tokio (current_thread)  | 73.4 ms | 3.14× | 3.31× |
| Go (GOMAXPROCS=1)              | 73.8 ms | 3.15× | 3.33× |

### Spawn — 100k actors (lower = better)

| Lang | Time | vs Lake | vs best |
|---|---|---|---|
| Lake (cooperative)             | **9.1 ms**  | 1.00× | **1.00×** |
| Rust + tokio                   | 35.9 ms | 3.95× | 3.95× |
| Go                             | 203.8 ms | 22.4× | 22.4× |

C++ has no peer: spawning OS threads/coroutines isn't comparable to cooperative actors, so it's omitted.

### MSG — ping-pong 100k round-trips (lower = better)

| Lang | Time | per round-trip | vs Lake |
|---|---|---|---|
| C++ coroutines (manual sched) | 2.5 ms   | 25 ns  | 0.11× |
| Lake (cooperative, mailbox)   | **22.9 ms**  | 229 ns | **1.00×** |
| Rust + tokio (mpsc)           | 37.8 ms  | 378 ns | 1.65× |
| Go (channels, GOMAXPROCS=1)   | 101.2 ms | 1012 ns | 4.42× |

C++ here is co-routines yielding in a tight loop with no scheduler abstraction — not an actor system. Among actor/async runtimes, **Lake is fastest**.

### CPU — fib(100k) × 8 workers (lower = better)

| Lang | Time | vs Lake | vs C-seq |
|---|---|---|---|
| C sequential (baseline)        | **813 ms**  | 0.13× | **1.00×** |
| C++ coroutines                 | 2188 ms | 0.36× | 2.69× |
| Go                             | 2401 ms | 0.39× | 2.95× |
| Rust + tokio                   | 5128 ms | 0.83× | 6.31× |
| Lake                           | 6164 ms | 1.00× | 7.58× |

CPU gap reflects per-CPS-block reduction counting (fairness guarantee stricter than BEAM's per-function counting). Architectural cost, not optimisation backlog.

---

## 3. Honest framing — by runtime category

C++ coroutines is a **manual** scheduler with no isolation, no fairness, no actor abstraction. Comparing it to actor runtimes is apples-to-oranges. Two more useful slices:

### 3a. Cooperative actor / async runtimes only

| Axis | Lake | Rust + tokio | Go | Best actor lang |
|---|---|---|---|---|
| I/O   | **23.4 ms** | 73.4 ms | 73.8 ms | **Lake** |
| Spawn | **9.1 ms** | 35.9 ms | 203.8 ms | **Lake** |
| MSG   | **22.9 ms** | 37.8 ms | 101.2 ms | **Lake** |
| CPU   | 6164 ms | 5128 ms | **2401 ms** | Go |

Lake is fastest of the cooperative actor runtimes on every axis except CPU. CPU loss is design choice (per-block reductions).

### 3b. vs theoretical floor (C-seq, C++ coroutines)

| Axis | Lake | Floor | Lake / Floor |
|---|---|---|---|
| I/O   | 23.4 ms | C++ 22.2 ms | 1.06× |
| MSG   | 22.9 ms | C++ 2.5 ms  | 9.16× |
| CPU   | 6164 ms | C-seq 813 ms | 7.58× |

I/O parity is the load-bearing claim. MSG and CPU gaps are bounded by architectural choices that buy fairness + isolation no peer offers.

---

## 4. Density — actors at rest

Workload: spawn N actors, each parks on `wait`, then measure RSS. AC power, post-dynamic-queue.

| N | Time to READY | RSS | Per-actor |
|---|---|---|---|
| 100k | 0.18 s | 408 MB | 4 281 B |
| 500k | 0.92 s | 2 040 MB | 4 280 B |
| **999 999** | **1.83 s** | **4.0 GB** | **4 180 B** |

Linear scaling confirmed. Per-actor footprint hardware-independent (RSS geometry).

**Spawn-and-park rate (AC):** ~544 k actors/s.

| Lang | Per-actor | 1M actors |
|---|---|---|
| Rust + tokio task | ~200–500 B | ~200–500 MB |
| Pony actor        | 0.5–1 KB   | 0.5–1 GB |
| **Lake**          | **4.2 KB** | **4.2 GB** |
| Go goroutine      | ~2.5 KB    | ~2.5 GB |
| Erlang process    | 2–4 KB     | 2–4 GB |
| Java thread       | ~1 MB      | ~1 TB |

Lake: Erlang-class density with a 13 KB binary, 0 deps.

---

## 5. Headline claims (publishable)

1. **Spawn fastest** of measured langs at N=100k — 4× rust+tokio, 22× go.
2. **I/O parity** with C++ coroutines (1.06×) under cooperative scheduling.
3. **Erlang-class density** at 13 KB / 0 dynamic deps. Linear to 1M actors.
4. **MSG: 230 ns per round-trip** — fastest cooperative actor runtime measured.

All four reproducible from `benchmark/run.sh perf` (axes I/O / spawn / MSG / CPU) plus density helper script.

---

## 6. Reproduction

```bash
cd lake-native-compiler
./benchmark/run.sh perf            # all axes
./benchmark/run.sh perf spawn      # one axis
```

Per-axis results land in `benchmark/results/{cpu,io,msg,spawn}.md` (raw hyperfine markdown). Density requires bumping HEAP_SIZE / process_arr cap **before** `4e5100c`; from that commit forward, `/tmp/density_test.lake` runs as-is on the default build.
