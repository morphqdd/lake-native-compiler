# Memory — allocator, fat-pointers, lifecycle

## Fat-pointer layout (16 bytes)

`rt/layout/fat_ptr.rs`.  Single struct used everywhere a heap-allocated
buffer is referenced.

```
+0   start   i64  — pointer to user data
+8   end     i64  — one-past-last byte (for bounds checks)
```

User code holds a **pointer to a fat-ptr struct**, not the struct
inline.  This is so a fat-ptr can be moved between actors (in mailbox
slots, in JUMP_ARGS, in VARIABLES) as a single i64 without losing
identity.

A `buf` / `str` / `i64`-returned-from-rt_allocate is **always** an 8-byte
address pointing at a 16-byte fat-ptr header somewhere on the heap.

## Bucket allocator

`rt/funcs/alloc.rs`.  Three paths:

```
size → log2_ceil(max(size, 16)) - 4 = bucket_idx ∈ [0, 20]
                                        for sizes 16 B … 16 MiB.

if bucket_idx ≤ 20:
  free_list[bucket_idx] != 0
    ? pop from free-list (zero payload), return
    : bump-allocate bucket_size from heap, return
else:
  direct mmap path (huge allocations > 16 MiB)
```

Bucket sizes are powers of two: 16, 32, 64, 128, ..., 16 MiB.  Each
allocation rounds up to the bucket size.  Internal fragmentation is
bounded by ≤ bucket_size / 2.

### Free-list pop

When a chunk returns to the free-list, its next-pointer is stored at
offset 0 of the payload.  On reuse, we unlink, **zero the payload**,
then hand it out.

The zero-init step (post-pop) was added recently (#45 surfacing bug).
Without it, recycled chunks retain previous owner's bytes — sha256.lake
hit a 4-bit-shifted hash because a leftover `0x01` byte appeared in a
padding zone build_padded expected to be zeroed by the allocator.

Cost: ~size/8 stores per pop.  Negligible vs the allocator's own
overhead.

### Bump path

Fresh allocation from the heap arena (mmap'd at startup).  The arena
is zero-initialized by the kernel (mmap of anonymous pages), so no
explicit zero needed.

### Huge path

For allocations > 16 MiB, bypass the bucket allocator and mmap a
fresh region per request.  The fat-ptr header lives at the start of
the mmap, so unmap-on-free works without a separate header table.

## User-code allocation lifecycle (current state)

User Lake code does **not** call `rt_free`.  Allocations live until the
owning actor's death.  When an actor reaches STOP_DONE:

`ShedulerCtxLayout::free_process_resources` frees:
- variables fat-ptr
- jump_args fat-ptr
- mailbox fat-ptr
- exec_ctx fat-ptr
- process_ctx fat-ptr

Anything the actor itself allocated (via `rt_allocate` in user code) is
**leaked** until the actor dies.  For short-lived actors (request
handlers) this is fine; for long-lived loops (TCP server, hot crypto
worker) it accumulates.

Foundational fix in #75 (per-actor heap + mark-sweep GC).  Until then,
the workaround is to scope long-running work into spawned children that
die after each unit of work.

## Bounds checking

Every memory access through an rt-function (rt_load_u*, rt_store,
rt_copy_bytes) reads the fat-ptr's `end` and traps if
`start + offset + size > end`.

Trap codes:
- 33 — destination out of range
- 34 — source out of range (rt_copy_bytes only)

Cranelift maps `trapz` to `ud2` on x86 → SIGILL.  The error surfaces as
"illegal instruction (core dumped)" from the binary.  No graceful
recovery — pure crash.  (Future: trap → kill actor, continue
scheduler.)

## Allocations that the runtime owns

The scheduler does its own heap usage independent of user code:

- `process_arr` — grows by doubling.  Each entry is a fat-ptr to a
  ProcessCtx (24 B).
- `wait_arr` — same shape.
- `io_parked` — fixed capacity 4096 (TODO: grow path).
- `pid_table` — 16 B per slot.  Grows by doubling.
- `free_slots` — stack of recycled pid slot indices.
- Each actor's vars/args/mailbox/exec_ctx/process_ctx allocations.

For a program with N peak concurrent actors:
- pid_table: ~32 KiB at N=2048
- process_arr at N=2048: 2048 × 8 = 16 KiB plus 1 ProcessCtx per actor (24 B).
- Per-actor: ~5 KiB total (vars 512 + args 2048 + mailbox 2048 + exec
  72 + process 24).
- Net memory at N=2048 active: ~10 MiB plus user-code allocations.

This is **far less** than the kernel-thread-per-actor alternative
(~8 MB stack per thread × 2048 = 16 GiB).
