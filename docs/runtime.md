# Runtime — scheduler, layouts, stop codes

## ExecCtx layout (72 bytes)

`rt/layout/exec_ctx.rs`.  One per actor.

```
+0   branch_id    i64  — which branch of the machine is active
+8   block_id     i64  — which CPS block inside the branch to resume at
+16  temp_val     i64  — scratch register for inter-block values
+24  variables    i64  — fat-ptr to variables array (let bindings, params)
+32  jump_args    i64  — fat-ptr to staged call args (transient)
+40  mailbox_fat  i64  — fat-ptr to ring-buffer mailbox (256 × 8 B)
+48  mailbox_head i64  — read index (consumer)
+56  mailbox_tail i64  — write index (producer)
+64  own_pid      i64  — this actor's pid (returned by `self` keyword)
```

Variables is **a pointer to a flat array of 8-byte slots**, not the
inline values.  Each Lake variable (named or `__pat_N`) gets a slot.
Wildcards and literal guards consume slots so positional indexing
stays consistent across branches with different pattern shapes.

JUMP_ARGS is the staging area for the next call's arguments.  The
caller writes args[i] into `jump_args[base + i]`; the callee's branch
reads them from VARIABLES (the spawner copies jump_args → variables).
TEMP_VAL is the scratch for rt-call return values that flow into a
subsequent arg position.

## ProcessCtx layout (24 bytes)

`rt/layout/process_ctx.rs`.  Pairs a function pointer with its ExecCtx.

```
+0   func_ptr        i64  — pointer to the compiled machine function
+8   exec_ctx        i64  — pointer to this actor's ExecCtx
+16  io_parked_idx   i64  — slot in scheduler's io_parked array (if parked)
```

The scheduler's process_arr stores `ProcessCtx*` (fat-ptr addresses,
actually).  A pid is a slot index into the **pid table** (see ShedulerCtx
PID_TABLE_*), not a direct pointer — this is what lets us recycle slots
safely via generation tagging.

## ShedulerCtx layout (224 bytes)

`rt/layout/sheduler_ctx.rs`.  Single global at module data section.

Significant fields:

```
+0    process_arr_fat       — fat-ptr to active actors queue
+8    current_process       — index of current actor in process_arr
+16   last_process_index    — high water mark
+24   real_count_of_processes — number actually alive
+32   wait_arr_fat          — fat-ptr to actors blocked on `wait`
+40   last_waited_process_index
+48   waited_process_count
+56   process_arr_cap       — current capacity (doubles on grow)
+64   wait_arr_cap
+72   io_uring_fd
+80-136 io_uring SQ/CQ tail/head/mask/array pointers
+144  io_parked_fat         — actors blocked on io_uring CQE
+152  io_parked_count
+160  io_parked_cap
+168  sqe_pending           — un-submitted SQE count
+176  pid_table_fat         — array of {gen i64, proc_ctx i64}
+184  pid_table_cap
+192  pid_table_len         — high water mark (slot 0 reserved as null)
+200  free_slots_fat        — stack of recycled slot indices
+208  free_slots_cap
+216  free_slots_len
```

## Scheduler main loop

`rt/scheduler/mod.rs`.  Built once into the program as the `_start`
entry point's main loop.

```
init_heap; io_uring_setup; init_main_process
loop:
  io_uring_poll_cq                  ← wakes parked actors on completion
  if real_count == 0:
    if io_parked_count > 0:
      io_uring_wait_cqe              ← blocks until something completes
      loop
    else if waited_count > 0:
      loop                            ← rely on mailbox sends to wake
    else:
      exit(0)
  current = process_arr[current_process]
  stop_code = call_indirect(current.func_ptr, current.exec_ctx)
  match stop_code:
    STOP_DONE  → free actor's heap, remove from queue, loop
    STOP_WAIT  → move to wait_arr, remove from process_arr, loop
    STOP_PARK  → already removed by rt_io_park_current, loop
    STOP_LIMIT → advance current_process (round-robin), loop
```

## Stop codes

`pipeline/machine.rs`:

```
STOP_DONE  = -1   process finished — scheduler reclaims memory
STOP_LIMIT = -2   quantum exhausted — scheduler picks next actor
STOP_WAIT  = -3   actor blocked on `wait` — move to wait_arr
STOP_PARK  = -4   actor parked on io_uring CQE — slot already vacated
```

## Mailbox protocol

Each actor's ExecCtx contains a ring-buffer mailbox at offset 40:

- `mailbox_fat` (16 B fat-ptr) → start of 256 × 8-byte slot array.
- `mailbox_head` — consumer index (next slot to read).
- `mailbox_tail` — producer index (next slot to write).

A send (`pid -> msg`):

1. Look up pid in scheduler's pid_table: extract slot (low 32 bits) +
   compare gen (high 32 bits).  Stale gen → silent drop.
2. Resolve to recipient's ProcessCtx → ExecCtx.
3. Write message slots into recipient's mailbox at `tail`, advance tail.
4. If recipient was in wait_arr, move them back to process_arr.

A wait:

1. Compile to a Cranelift block that reads mailbox starting at `head`.
2. If empty: store BLOCK_ID, return STOP_WAIT (yield to scheduler).
3. If filter present: skip messages whose sender pid isn't in filter list.
4. On match: bind handler patterns to message slots, advance head, continue
   into handler body.

## Generation-tagged pids (#74)

Naive pid = process_ctx pointer.  Problem: actor dies, memory recycled,
new actor reuses the pointer — old senders' messages misroute.

Generation tagging:

- pid = `(gen << 32) | slot` where slot ∈ [0, pid_table_len).
- `pid_table[slot] = { gen, proc_ctx_fat_ptr }`.
- On send: extract slot from pid, compare incoming gen with
  pid_table[slot].gen.  Mismatch → drop.
- On actor death: increment pid_table[slot].gen, push slot to free_slots.
- Slot 0 is the null-pid sentinel.

Bounded memory: peak concurrent live actors caps pid_table size.

## rt-functions (rt/funcs/)

Built into every Lake program by the compiler.  Each is a Cranelift
function emitted alongside user machines.  User code declares the ones
it uses via `@rt(name)`.

Key ones:

- `rt_allocate(size) -> buf` — bucket allocator, see [memory.md](memory.md).
- `rt_free(buf)` — push back to free-list.
- `rt_store(buf, val, size, offset)` — bounds-checked byte/u16/u32/u64 store.
- `rt_load_u8` / `_u16` / `_u32` / `_u64` — bounds-checked loads.
- `rt_copy_bytes(dst, dst_off, src, src_off, len)` — bounds-checked memcpy.
- `rt_write(fd, buf, len)` — direct write syscall.
- `rt_write_async(fd, buf, len)` — io_uring submit, parks actor on CQE.
- `rt_exit(code)` — exit syscall.

## syscall.o

Embedded blob (`include_bytes!("syscall.o")`) with raw x86_64 syscall
trampolines (no libc).  Linked into every output binary.  Object file
path is written to `build_path` at link time so `lakec` runs from any
CWD.
