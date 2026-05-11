# Concurrency model

## Quantum

`pipeline/machine.rs::compile_machine` initializes `quantum_var = 256`
on entry.  Each CPS block decrements it; on zero the machine returns
`STOP_LIMIT`, scheduler picks next actor.

The number 256 was chosen empirically: CPU-bound benchmarks went
`640 ms → 260 ms → 28 ms` as the quantum tightened.  Larger quantum =
better cache locality, smaller = better fairness.  256 is the sweet
spot for current workloads.

## Reduction counting

Every CPS block = one "reduction" (BEAM term).  A machine running a
loop via `self()` increments BLOCK_ID per CPS-block, which fires a
quantum decrement, which after 256 returns control.

So a tight loop of N iterations costs ~N reductions.  Any actor can
be rotated out at most 256 reductions after entering.

## What counts as a "yield point"

The scheduler-visible yield points (from a Lake program's perspective):

- **Quantum exhaustion** (STOP_LIMIT): every 256 CPS blocks, mandatory.
- **`wait` block** (STOP_WAIT): explicit `wait { ... }` or implicit via
  the `let r = ret_machine_call(...)` desugar.  Actor moves to wait_arr,
  unblocks when a matching message arrives.
- **`rt_io_park_current`** (STOP_PARK) and similar: async I/O calls
  (`rt_write_async`, `rt_send_async`, `rt_accept_async`, `rt_recv_async`)
  park the actor on io_uring's CQE.  Actor wakes when the kernel
  signals completion.

Non-yield points (don't rotate scheduler):

- Arithmetic, bitwise, comparisons
- Local `let` bindings
- `when` dispatch (compiles to Cranelift Switch, no yield)
- Direct calls to rt-functions (rt_allocate, rt_load_*, rt_store,
  rt_copy_bytes, rt_write_sync, rt_exit, rt_mmap, rt_munmap)
- `self()` state transition — this is NOT a spawn, it's a branch+block
  reset.  Cheap, single store + jump.

## Why a ret-machine call yields

A `let r = f(args)` where `f` is a ret-machine is **semantically actor
spawn + wait**.  Lowering rewrites it to:

```
let __ret_N_pid pid = f(self, args)
wait __ret_N_pid { ... r ty -> rest }
```

The `wait` is a real yield point: scheduler runs other actors until the
spawned child replies via mailbox.  Even when no other actors exist,
this round-trip costs allocation + scheduler dispatch + 2 extra CPS
blocks (~2 μs).

This is the bulk of overhead in CPU-bound Lake code.  SHA-256 of 1 MiB
takes 35 s in Lake vs 7 ms in C — almost entirely from the ~16 M
ret-machine calls per hash.

## Fairness guarantees

**For all actors A, B in process_arr**:
After A enters its compiled function, control returns to the scheduler
within ≤ 256 CPS blocks.  Therefore B will execute its quantum within
N × 256 reductions where N = process_arr.len.

This holds **regardless of what A does** — pure compute, mailbox sends,
allocations, all increment reductions.

**Exceptions** (where fairness can be violated):

- A non-yielding rt-call that itself runs unbounded.  None currently
  exist; if added, must increment reduction or split into chunks.
- A future "pure ret-machine inline" (#78) that compiles a tight loop
  to native code without per-iteration reduction counting.  Phase-1
  design excludes self-recursive pure precisely to keep fairness.

## Cooperative I/O

Actors don't block the scheduler thread on I/O.  Async I/O calls submit
an SQE to io_uring, then call `rt_io_park_current` which:

1. Stores the resume block ID into ExecCtx.
2. Removes the actor from process_arr.
3. Adds to io_parked array with the actor's user_data (= proc_ctx ptr).
4. Returns STOP_PARK to scheduler.

When io_uring delivers a CQE, the scheduler's `rt_io_uring_poll_cq`
finds the matching user_data, looks up the parked actor, and moves it
back to process_arr.

The result: 8 actors hashing 128 KiB each finish in the same wall time
as 1 actor hashing 1 MiB (≈35 s) — total CPU work is identical, but no
context-switch overhead.

## Observed scaling

Benchmark: SHA-256 of 1 MiB total work, varying concurrency:

```
1 actor    × 1 MiB   = 35.20 s
8 actors   × 128 KiB = 34.86 s  (-1.0%)
64 actors  × 16 KiB  = 35.30 s  (+0.3%)
256 actors × 4 KiB   = 35.16 s  (-0.1%)
1024 actors × 1 KiB  = 36.82 s  (+4.6%)
```

Scheduling overhead is **noise-level** up to ~256 concurrent actors,
~5% at 1024.  At 1024 the per-actor work approaches the quantum size,
so allocation/death dominates per-actor cost.

This is the language's main quantitative selling point: cooperative
scheduling at scale, **on a single thread**, with overhead that doesn't
grow with N.  C/Rust would need either threads (per-actor kernel cost)
or async runtime (per-poll cost); both lose linearly with N.

## What we don't have

- **Multi-core scaling**: scheduler is single-threaded.  Multi-core
  needs per-core process queues, work stealing, lock-free mailbox.
  Major undertaking; not on near-term roadmap.
- **Preemption**: actors can't be interrupted mid-block.  A bug that
  causes an actor to spin in a non-yielding loop hangs the scheduler.
  Quantum guarantee assumes well-formed CPS lowering.
- **Priority scheduling**: round-robin only.  Adaptive quantum (small
  for I/O-bound, large for CPU-bound) would help responsiveness under
  mixed load; not implemented.
