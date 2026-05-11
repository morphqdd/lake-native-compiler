# Architecture

Lake is an **always-async, actor-based language**.  There is no syntactic
distinction between synchronous and asynchronous code — every function is
a cooperative process running under one shared scheduler.

The compiler targets native code via Cranelift JIT/AOT.  No libc — direct
syscalls only.  Binary sizes: 6-10 KB for a minimal program.

## Three layered concepts

### 1. Machine (= function)

A Lake top-level declaration:

```lake
counter is {
  n i64 -> {
    when n {
      0 -> { }
      _ -> { self(n - 1) }
    }
  }
}
```

A **machine** is the static description of a function-like callable.  At
runtime, machines become Cranelift functions.

There are two kinds:

- **Spawn-style machine**: no `-> ret <type>` annotation.  Called by name
  to **start a new actor** (no value is returned to the caller).
  Example: `counter` above.  Calling `counter(10)` spawns a child actor
  running the counter logic.

- **Ret-machine**: has `-> ret <type>`.  Called for its value.
  ```lake
  rotr32 is {
    x i64 n i64 -> ret i64 { ret x >> n | (x << (32 - n)) & 0xffffffff }
  }
  ```
  Calling `rotr32(x 5)` reads as "spawn a child actor, wait for it to
  reply with the value".  Mechanically this is **still actor-spawn** —
  the synchronous-looking syntax is sugar over mailbox round-trip.  See
  [lowering.md](lowering.md).

### 2. Actor (= running process)

An actor is a machine **in execution**.  Each actor has:

- An `ExecCtx` (72 bytes) tracking its current branch, block id, scratch,
  variables, jump args, mailbox, own pid.
- A `ProcessCtx` (24 bytes) wrapping a function pointer + ExecCtx.
- A pid — generation-tagged slot index into the scheduler's pid table.

Actors communicate **only** via mailbox sends (`pid -> message`).  No
shared mutable state is observable across actor boundaries.

### 3. Scheduler (= main loop)

A single cooperative round-robin scheduler.  Each scheduler tick picks
the next runnable actor and calls its compiled function.  The function
runs up to `QUANTUM = 256` CPS blocks before yielding control back.

```
loop:
  if io ring has completions → wake parked actors
  if process_arr empty → exit
  current = process_arr[i]
  stop_code = call_indirect(current.func, current.exec_ctx)
  switch stop_code:
    STOP_DONE  → free actor, remove from queue
    STOP_LIMIT → advance i (round-robin)
    STOP_WAIT  → move to wait_arr, advance
    STOP_PARK  → already removed by rt_io_park_current
```

## CPS execution model

Every Lake expression compiles to a **CPS block** — a Cranelift basic
block that ends with `jump(quantum_continue_block, next_block_id)` rather
than continuing straight to the next instruction.

```
expr_block_N:
  ... compute expression's effect ...
  jump quantum_continue, [next_id = N+1]
```

`quantum_continue_block` (per machine function) decrements a quantum
counter, checks for stop codes, and either re-dispatches via the per-
branch switch (if quantum left) or returns STOP_LIMIT to the scheduler.

This makes scheduling **fair**: no actor can hog CPU for more than 256
expression evaluations before yielding.

The cost: each expression evaluation pays a dispatch round-trip
(`store BLOCK_ID; decrement; check; jump`).  Pure compute pays this even
when it doesn't need to yield, which is why CPU-heavy code (SHA-256) is
~8000× slower than C.  See [concurrency.md](concurrency.md) and #80
(CPS coalescing) for the planned mitigation.

## Why this architecture

Three benefits we exploit:

1. **Scaling concurrency without thread overhead.**  Spawning a Lake
   actor costs ~5 KB heap.  Spawning a pthread costs ~8 MB stack +
   kernel scheduling.  1024 concurrent Lake actors fit in 6 MB; 1024
   pthreads do not.

2. **No-thread-needed I/O concurrency.**  Single-thread cooperative
   scheduling + io_uring parking means a hash actor and a TCP accept
   actor coexist on one CPU thread with `~0%` scheduling overhead
   (benchmarked: 1 sequential hash vs 256 concurrent hashes of 1 MiB
   total: 35.20s vs 35.16s — noise-level).

3. **Fairness as a language guarantee.**  Quantum + reduction counting
   means a poorly written tight loop in one actor can never starve
   others.

## What we trade

- **Per-task throughput** is 100-8000× slower than tuned natives on CPU-
  bound code.  Architectural fix: #78 (inline pure ret-machines) + #80
  (CPS coalescing).
- **No GC** yet — user code can't `rt_free`, allocations live until
  process death.  Foundational fix: #75 (per-actor heap + mark-sweep).
- **No first-class machines** — machines are static names only, not
  values.  Affects expressiveness of generic libraries / collections.
  See discussion in #76 / #77 / lake-RoR (#52).
