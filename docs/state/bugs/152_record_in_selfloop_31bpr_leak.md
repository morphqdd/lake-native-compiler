# Bug 152 — 31 B/req leak: record creation in a self-looping async actor

**Status:** fixed (3c516ff)  **Severity:** medium  **First seen:** 2026-05-28
**Fixed:** 2026-05-28 — see `## Fix` section.
**Reproduces under:** `LAKE_SLAB_ALLOC=1`.  Default bucket mode: 0 leak.

## Symptom

When a non-ret machine self-loops via `self(...)` and the loop body
allocates a record on the actor's arena, RSS grows linearly at
**~31 B/req** under slab mode.  Without the record allocation the
same loop is leak-free.

## Repro

`/tmp/server_min.lake`:

```
+std.tcp.{ listen accept send close }

Connection is {
  conn i64
}

handle is {
  fd i64 -> {
    let conn = accept(fd)
    let c Connection = Connection(conn)
    serve(c)
    self(fd)
  }
}

serve is {
  c Connection -> {
    pin send(c.conn "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
    pin close(c.conn)
  }
}

main is {
  _ -> { let fd = listen(8061)  handle(fd) }
}
```

Build + measure:

```
LAKE_SLAB_ALLOC=1 STD_PATH=/home/morphe/compiler/lake-stdlib/std \
  /home/morphe/compiler/lake-native-compiler/target/release/lakec \
  -O speed /tmp/server_min.lake -o /tmp/min
/tmp/min/server_min &
oha --no-tui --disable-keepalive -n 10000 -c 1 http://127.0.0.1:8061/
```

`VmRSS` grows 668 KB → 972 KB over 10 000 sequential GETs.

## What's been ruled out

- **Slab metadata**: every `slab_class_state` mutation has a strict
  inverse on `rt_free_slab` (verified by reading
  `/home/morphe/compiler/lake-native-compiler/src/compiler/rt/funcs/alloc.rs:2287-2671`).
  Slab-header + bitmap live inside the slab payload — vanish on
  `munmap`-on-empty.  Disabling munmap-on-empty (diagnostic patch,
  reverted) did **not** eliminate the leak.
- **pid_table / free_slots growth**: initial cap is 4096 entries each
  (`sheduler_ctx.rs:116`), peak working-set is ≤ 3 actors, grow path
  never fires.  Pop/push pairs are balanced.
- **rt_copy_to_arena (Phase 2d arg copy)**: the leak appears when
  `serve(c.conn)` is called with an i64 arg too, so the buf-typed
  arg copy path is NOT the cause.  Bypassing the copy entirely
  (`stored = val` always) still leaks 31 B/req.
- **rt_copy_to_arena fallback path**: trap planted in the
  fallback branch never fires — server processed 100 req without
  crashing, confirming `has_arena_block` always takes the bump
  path and never falls through to the `rt_allocate_raw` leak.
- **Bucket bypass**: `define_allocate_impl` early-returns at
  `alloc.rs:398` in slab mode; legacy bump path is unreachable.
- **rt_init_heap 4 GiB reserve**: `smaps` shows +8 KB across 20 k
  requests — orders of magnitude below the observed leak.

## Where the leak appears in smaps

Diffing `/proc/$PID/smaps` warm → after 20 000 req:

```
7fb8a1980000   12 KB → 68 KB  (+56 KB)
7fb8a1a40000   ∅   → 56 KB    (new)
7fb8a1a60000   ∅   → 64 KB    (new)
7fb8a1a80000   ∅   → 68 KB    (new)
7fb8a1ae0000   ∅   → 64 KB    (new)
```

Every growing region is at a 64-KiB-aligned base — they are slab
mmap'd regions that should have been reclaimed via
`rt_free_slab → munmap` but **stay mapped**.  Pattern fits "one
chunk per slab is perpetually allocated and pins the slab".

## Diagnostic — per-class slab counters

Diagnostic patch (since reverted) added two global counter arrays:
`slab_class_active[21]` (incremented in `rt_allocate_slab`,
decremented in `rt_free_slab`) and
`slab_class_lifetime_allocs[21]` (alloc-only).  Counter dump via
`kill -ABRT` → `coredumpctl dump` → ELF reader against the binary
+ core symbol table.

For `server_min` (record + Connection arg, 5000 req, RSS 660 →
800 KB):

```
class  size       allocs   frees   active   pinned_bytes
[ 1]  32         7352     5041    2311     73952
[ 2]  64         30254    30250   4        256
[ 3]  128        5041     5041    0        0       <- balanced
[ 4]  256        20169    20167   2        512
[ 8]  4096       40336    40332   4        16384
[12]  65536      3        0       3        196608  <- live actor arenas
[13]  131072     5045     5042    3        393216
```

Class 1 alone leaks 2311 chunks (74 KB), which fills two full
class-1 slabs (~2030 chunks/slab × 32 B = 64 KB each).  The
remaining 4 + 2 + 4 = 10 active chunks across classes 2/4/8 are
the running actors' (handle, serve, etc.) bookkeeping — those
stay until process exit.

For `server_spawnonly` (no record, 5000 req): every alloc is
balanced — class 1 = 0 leak.

For `server_recok` (record + i64 spawn, no Connection arg-copy):
class 1 = 2271 leaked.  Same magnitude as `server_min` — confirms
the leak source is the record **creation** path, not the
async-spawn arg copy.

## Root cause confirmed: `rt_arena_alloc` fallback path leaks

Added `arena_alloc_fallback_count` counter to the fallback branch
of `rt_arena_alloc` (`alloc.rs:1538-1546`, since reverted).
After 5000 req on `server_min`:

```
class[1]   2298 active chunks
fallback_count = 2298
```

**1:1 match** — every `rt_arena_alloc` → fallback call leaks a
class-1 chunk.  The fallback calls `rt_allocate_raw(user_size)`
and returns the result, but the caller (`tuple_expr.rs:96`,
`change_state_expr.rs:271`) treats the result as arena-lifetime
memory and never issues `rt_free`.

`rt_arena_alloc` body (`alloc.rs:1481-1545`):

* `has_arena_block` (bump from arena cursor): used when
  `target_proc_ctx.OWNED_ARENA_FAT != 0` and `new_bump <= arena_end`.
* `fallback_block`: when either check fails — calls
  `rt_allocate_raw(user_size)`.

The fallback branch was originally a guard against arena
exhaustion / missing arena.  In the slab-allocator era, every
spawn (including `main` via `init_main_process`) gives the actor
an arena, so the "missing arena" case should never fire for
user code.  Yet we see ~0.46 fallback calls per request.

## Open question

Why does the `arena_fat == 0` check fire ~0.46×/req for record
creation but never for non-record-bearing flows
(`server_spawnonly` = 0 fallback)?

Trace seemed to rule out:

* handle's OWNED_ARENA_FAT cleared mid-iteration (`self()`
  doesn't touch it).
* CURRENT_PROCESS pointing at a freed actor (would crash
  elsewhere).
* arena exhaustion (`self()` resets bump each iteration).

Likeliest remaining explanation: a specific code path inside the
spawn machinery (e.g. `accept`'s sync ret return, the `let conn =
accept(fd)` wait, or the let-binding for `c` itself) executes
under a transient scheduler state where `CURRENT_PROCESS` points
at an actor whose `OWNED_ARENA_FAT` slot is uninitialised or
stale.

## Suggested next step

Plant a side-channel write inside the fallback path that captures
the calling `CURRENT_PROCESS` index, the loaded `proc_ctx`
address, and the actor's machine name table offset.  Cross-
reference with a process-id-to-machine table dumped at the same
time to identify which actor is hitting the fallback.

## Fix (commit 3c516ff)

Root cause: `compile_fused_self_call` in
`src/compiler/pipeline/expr/jump_expr.rs:51-66` is the fast path
for `self(...)` calls whose args are all pure (Var, Num, atom, …).
It bypasses `change_state_expr::compile` — the only place that
runs the #138-phase-2e arena-reset sequence.  Self-looping non-ret
actors with any in-body arena allocation (record / tuple literal)
therefore bumped their arena every iteration until the 64 KiB
exhausted, then every subsequent `rt_arena_alloc` fell back to
`rt_allocate_raw`, pinning class-1 slab chunks forever.

The diagnostic counters (instrumentation since reverted) on
`/tmp/server_min`:

```
[0] arena_fat == 0:    0
[1] arena exhausted:   2288   ← every fallback was exhaustion
[2] bump success:      2730
[3] reset fires:       0      ← never! slow path skipped
[4] no-ptr-args path:  0
[6] change_state hit:  0      ← entire fn never called
```

Fix: emit the same reset sequence inline inside
`compile_fused_self_call` between the vars-write step and the
BRANCH_ID set.  Guarded by `OWNED_ARENA_BASE != 0` so
sync-ret-machine inheritance is unaffected.

## Measured impact

| Workload                     | Before    | After     |
|------------------------------|-----------|-----------|
| `server_min` (record + spawn)| 31 B/req  | 0.8 B/req |
| `server_recok` (rec + i64)   | 31 B/req  | 0 B/req   |
| `server_spawnonly` (no rec)  | 0         | 0         |
| lake-server default bucket   | 110 B/req | 80 B/req  |
| lake-server LAKE_SLAB_ALLOC=1| 95 B/req  | 64 B/req  |

`server_spawnonly` was already 0 because no record creation
means no arena bump means no exhaustion regardless of reset.

## Followup — lake-server still leaks 64 B/req

Diagnostic on lake-server (5000 req, slab mode):

```
class[ 1]:  5001 allocs,  5001 frees, 0    active
class[ 2]: 350008 allocs, 345005 frees, 5003 active  ← ~1/req leak
class[ 4]: 295006 allocs, 295004 frees, 2 active
class[ 8]: 530010 allocs, 530006 frees, 4 active
class[12]:      3 allocs,      0 frees, 3 active (live arenas)
class[13]:  15004 allocs,  15001 frees, 3 active (live arenas)

spawns: 265004 / deaths: 265003 — balanced (1 = listener never dies)
```

So spawn/death are balanced; the leak is NOT a missed
`free_process_resources`.  ~1 class-2 (40-48 B user_size) chunk
per request leaks somewhere outside the spawn lifecycle.

Candidates:
- `rt_copy_to_arena` fallback path (still leaks `rt_allocate_raw`).
- `change_state_expr`'s scratch alloc for pointer-arg snapshot
  (line 211: `rt_allocate_raw(total_size)` — should be freed at
  line 295, but maybe not every iteration?).
- An arena-allocation that falls back when serve_file's arena
  exhausts on big-file streaming.

Tracked as task #76.

## Workaround

Until root cause is fixed: use default bucket allocator (omit
`LAKE_SLAB_ALLOC=1`).  Bucket has the same overall budget but no
slab-pinning effect (no munmap-on-empty to be blocked).
