# io_uring integration — design

## Goal

Replace synchronous `rt_write` / `rt_read` with async-by-default I/O backed by
io_uring.  Lake actors that hit I/O are parked on submit and woken by the
scheduler when the kernel reports completion — no thread blocks on a syscall.

Target Linux kernel: **5.6+** (covers `IORING_OP_WRITE`, `IORING_OP_READ`,
`IORING_FEAT_NODROP`).  No fallback to `read(2)`/`write(2)` — Lake assumes
modern Linux.

## Non-goals (this iteration)

- `accept` / `connect` / network ops (deferred — needs sockets in language)
- Fixed buffers (`IORING_REGISTER_BUFFERS`)
- Polled mode (`IORING_SETUP_SQPOLL`)
- Multiple rings (single global ring per scheduler is enough single-threaded)
- Ring grow (256 SQEs is the cap — io_uring requires power-of-two fixed size)

## Architecture

```
                 ┌────────────────────────────────────────┐
                 │            Lake scheduler              │
                 │                                        │
   process_arr ──┤  active actors (round-robin)           │
                 │                                        │
                 │  ┌───────────────────────────────┐     │
                 │  │ scheduler_loop:               │     │
                 │  │   poll_cq()  ←── new          │     │
                 │  │   pick next process           │     │
                 │  │   exec block (up to 256)      │     │
                 │  │   on STOP_DONE → free + remove │    │
                 │  │   on STOP_LIMIT → next        │     │
                 │  │   on PARK_IO    → io_park ←── new   │
                 │  └───────────────────────────────┘     │
                 │                                        │
   io_parked ────┤  actors waiting on CQ                  │
                 │  layout: [(process_ctx_ptr, sqe_idx)]  │
                 └────────────────────────────────────────┘

                          │     │     │
                  io_uring_fd  SQ ring  CQ ring  SQE array
                          │   (mmap)  (mmap)  (mmap)
                          ▼
                       ┌────────────────────────┐
                       │       Linux kernel     │
                       └────────────────────────┘
```

## ShedulerCtx layout extension

Add 6 new fields after the dynamic-queue fields (offsets 88..136), bumping
`SIZE` from 88 to 136:

```
+88   IO_URING_FD        (i32, signed) — kernel ring fd
+96   SQ_RING_PTR        — mmap base of SQ ring (read head/tail/etc here)
+104  CQ_RING_PTR        — mmap base of CQ ring
+112  SQE_ARRAY_PTR      — mmap base of SQE array (64-byte entries)
+120  IO_PARKED_FAT      — heap fat-ptr to (process_ctx, sqe_idx) pairs
+128  IO_PARKED_COUNT    — current count
```

(`IO_PARKED_CAP` lives implicitly — the heap fat-ptr's end - start gives
capacity in bytes, divided by 16 = pair count.  Or add explicit `IO_PARKED_CAP`
as the dynamic-queue helper expects, totalling 144 B.)

Initial allocation: 64 pairs × 16 B = 1 KiB heap.  Doubles on demand using the
same `emit_grow_array_if_full` helper as `process_arr`/`wait_arr`, with stride
16 (instead of 8).  Helper needs minor parameterisation for stride.

## Boot sequence — `rt_io_uring_setup`

Call once from `_start` after heap is initialised, before main process spawn.

```rust
struct io_uring_params {
    sq_entries: u32,           // OUT
    cq_entries: u32,           // OUT
    flags: u32,                // IN — 0 (no SQPOLL, no IOPOLL)
    sq_thread_cpu: u32,        // unused
    sq_thread_idle: u32,       // unused
    features: u32,             // OUT — verify IORING_FEAT_NODROP set
    wq_fd: u32,                // unused
    resv: [u32; 3],
    sq_off: io_sqring_offsets, // OUT — 40 B
    cq_off: io_cqring_offsets, // OUT — 40 B
}
// Total: 120 bytes
```

1. Stack-allocate `io_uring_params` (zero-init).
2. `fd = syscall(SYS_io_uring_setup=425, 256, &params)`.
3. Trap if `fd < 0`.
4. mmap three regions using offsets from kernel:
   - SQ ring: `mmap(NULL, sq_off.array + 256*4, PROT_RW, MAP_SHARED|MAP_POPULATE, fd, 0)`
   - CQ ring: `mmap(NULL, cq_off.cqes + 256*16, PROT_RW, MAP_SHARED|MAP_POPULATE, fd, 0x8000000)`
   - SQE array: `mmap(NULL, 256*64, PROT_RW, MAP_SHARED|MAP_POPULATE, fd, 0x10000000)`
5. Store all four (fd + 3 mmap addresses) in ShedulerCtx fields.

## Submit path — `rt_write_async(fd, fat_ptr, size)`

Replaces `rt_write` entirely.  Sync `rt_write` kept as `rt_write_sync` for
debug/static buffer use.

```
rt_write_async(fd, fat_ptr, size):
    1. start = fat_ptr.start  (existing bounds check)
    2. tail = SQ.tail          (acquire load)
    3. mask = SQ.ring_mask
    4. sqe_idx = tail & mask
    5. sqe = SQE_ARRAY[sqe_idx]
    6. sqe.opcode    = IORING_OP_WRITE  (1 byte)
    7. sqe.flags     = 0                (1 byte)
    8. sqe.ioprio    = 0                (2 bytes)
    9. sqe.fd        = fd               (4 bytes)
   10. sqe.off       = 0                (8 bytes; ignored for non-seekable)
   11. sqe.addr      = start            (8 bytes)
   12. sqe.len       = size             (4 bytes)
   13. sqe.rw_flags  = 0                (4 bytes)
   14. sqe.user_data = current_proc_ctx (8 bytes)
   15. zero rest of SQE
   16. SQ.array[sqe_idx] = sqe_idx      (the kernel reads indirectly)
   17. SQ.tail = tail + 1               (release store)
   18. syscall(SYS_io_uring_enter=426, fd, 1 /*to_submit*/, 0 /*min_complete*/, 0 /*flags*/, 0, 0)
   19. io_park_current_actor(user_data=current_proc_ctx)
   20. return STOP_PARK = -3   (new stop code: actor has been moved to io_parked)
```

The `STOP_PARK` stop code tells the scheduler to skip past `remove_current_process`
(it's not done) and skip `next_process` (the slot was already vacated by
`io_park_current_actor` via swap-and-pop).  Just continue the loop.

### `current_proc_ctx` as `user_data`

io_uring's `user_data` is a 64-bit value the kernel returns verbatim in the
CQE.  Using the `ProcessCtx` fat-ptr as `user_data` lets us wake the actor
directly without an indirection table — no `sqe_idx` lookup needed at wake
time.  ProcessCtx is heap-allocated and stable for the actor's lifetime.

## Park — `io_park_current_actor(user_data)`

```
1. swap-and-pop current process from process_arr (existing
   `remove_current_process` logic — extracted as `unlink_current_process`
   that does NOT free the resources)
2. emit_grow_array_if_full(IO_PARKED_FAT, IO_PARKED_CAP, count+1)
   (same helper, stride=16)
3. write (user_data, 0 /*sqe_idx unused*/) at io_parked[count]
4. count += 1
5. REAL_COUNT_OF_PROCESSES -= 1
   (so scheduler exit condition fires only when ALL processes are
   either done or parked — for I/O-bound programs, scheduler must
   keep polling CQ until parked count drops back to 0)
```

## CQ poll — `rt_io_uring_poll_cq`

Called once per outer scheduler iteration, before picking next process.

```
1. head = CQ.head        (no barrier needed — we own head)
2. tail = CQ.tail        (acquire load)
3. while head != tail:
     cqe_idx = head & CQ.ring_mask
     cqe = CQ.cqes[cqe_idx]
     user_data = cqe.user_data
     // res = cqe.res — error code or bytes written; ignored for now
     io_wake_actor(user_data)
     head += 1
4. CQ.head = head        (release store)
```

### `io_wake_actor(user_data)`

```
1. Linear scan io_parked for a row with .ptr == user_data
   (worst case 256, fine for now; future: hash table)
2. swap-and-pop from io_parked
3. emit_grow_array_if_full(PROCESS_ARR_FAT, PROCESS_ARR_CAP, count+1)
4. write user_data at process_arr[count]
5. count += 1
6. REAL_COUNT_OF_PROCESSES += 1
```

## Termination

Scheduler exit condition currently: `REAL_COUNT_OF_PROCESSES == 0`.

After io_uring: parked actors decrement `REAL_COUNT`, so a program with all
actors parked on I/O would exit prematurely.  Two fixes possible:

**Option A (minimal):** keep `REAL_COUNT` decremented for parked actors, but
add a separate condition: scheduler also exits only if `IO_PARKED_COUNT == 0`.
Simple; if both are zero, all actors are gone.

**Option B (cleaner):** keep `REAL_COUNT` including parked.  A parked actor
still "exists", just isn't runnable.  Scheduler tick:
- If `runnable_count > 0`: pick next from process_arr, run quantum
- Else if `IO_PARKED_COUNT > 0`: blocking `io_uring_enter(min_complete=1)` to
  wait for at least one CQE, then poll CQ
- Else: exit

Option B preferred — it gives us **proper kernel-level blocking** when the
process has nothing to do but wait for I/O, instead of busy-spinning poll_cq
in user space.

## Memory ordering

io_uring rings are SPSC between user-space (us, single-threaded) and kernel.
On x86-64 with the kernel ABI:

- **SQ tail write** (we produce): release semantics → in C: `smp_store_release`,
  in our IR: regular store + compiler fence.  On x86 store-store is already
  ordered, but Cranelift may reorder loads/stores around it; emit a no-op fence
  via inline asm or — simpler — use `MemFlags::trusted()` and rely on the fact
  that the next instruction is a syscall (full memory barrier).
- **CQ head write** (we consume, kernel reads): same — syscall provides barrier.
- **CQ tail read** (we consume, kernel produces): acquire semantics.  On x86
  load-load is already ordered.  Use `MemFlags::trusted().with_volatile()` to
  prevent Cranelift re-ordering.

For the MVP we rely on:
1. The `io_uring_enter` syscall as the natural sync point for SQ.
2. `MemFlags::trusted()` reads for CQ tail.

If we ever pursue SQPOLL mode (kernel poll thread), proper barriers become
mandatory and this design needs revisiting.

## Stop codes

Existing:
- `STOP_DONE   = -1` — process finished
- `STOP_LIMIT  = -2` — quantum exhausted

New:
- `STOP_PARK   = -3` — actor parked on I/O, slot already vacated, scheduler
  should not call `remove_current_process`

Scheduler dispatch:
```
result = exec_block(process)
match result:
  STOP_DONE:  free_process_resources; remove_current_process
  STOP_LIMIT: advance current process index
  STOP_PARK:  no-op (slot already vacated), continue to poll_cq + next iter
  else:       (block id) — bug, trap
```

## Files affected

| File | Change |
|---|---|
| `src/compiler/rt/funcs/mmap.rs` | + io_uring offset constants, MAP_SHARED + MAP_POPULATE flags |
| `src/compiler/rt/funcs/io_uring.rs` | NEW — setup, write_async, poll_cq, park, wake |
| `src/compiler/rt/layout/sheduler_ctx.rs` | + 6 fields (88..136), grow helper stride param |
| `src/compiler/rt/scheduler/mod.rs` | + STOP_PARK case, + poll_cq before next pick, + Option B termination |
| `src/compiler/rt/mod.rs` | + define_io_uring_setup before init_heap |
| `src/compiler/ctx/rt_funcs.rs` | + io_uring fn ids |

## Risks & open questions

1. **SQE 64-byte alignment in Cranelift.** `MemFlags::trusted()` should suffice;
   verify with disasm that no tearing occurs.
2. **CQ ring offset hack (`0x8000000`)** is the documented `IORING_OFF_CQ_RING`
   magic offset to mmap.  Must use exact value or kernel returns EINVAL.
3. **`io_wake_actor` linear scan** is O(N parked).  Acceptable up to ~thousands;
   needs hash table for >10k concurrent I/O.  Defer.
4. **Cancel-on-death.** If an actor parked on I/O dies (e.g. parent killed),
   the SQE may still complete after the actor is freed.  Need IORING_OP_ASYNC_CANCEL
   or accept that the CQE just gets dropped (since user_data points to freed memory,
   io_wake_actor scan won't find it — already safe by accident).  Document, defer.
5. **`io_uring_setup` failure on systems without kernel 5.6+** must be a clear
   error message, not a SIGSEGV.  Trap with custom code.

## Acceptance criteria for stage 6

- 28+1 integration tests pass (existing 28 + new `actor_writes_via_io_uring`)
- I/O bench wall time decreases vs sync rt_write (target: 5-10 ms vs 23 ms)
- System time (kernel) drops vs sync (target: 13 → 5-7 ms via batching)
- No SIGSEGV / EINVAL on `io_uring_setup` (binary still runs to completion
  on standard distro kernels)
- New SUMMARY.md numbers + lake-thesis log.md AC entry with date

## Stage breakdown — see tasks #50-#55

1. Design doc + scaffolding (this file + STOP_PARK constant + stub setup fn)
2. Real ring setup with mmap
3. rt_write_async submit path
4. Park/wake list + grow helper stride param
5. CQ poll integration in scheduler + Option B termination
6. Tests + bench + docs
