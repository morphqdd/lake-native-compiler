# Bug 152 — 31 B/req leak: record creation in a self-looping async actor

**Status:** open  **Severity:** medium  **First seen:** 2026-05-28
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

## Hypothesis (unverified)

Some per-request allocation lands in a small slab class (0..6), is
counted as "in use" but never reaches the `free_count == chunks_per_slab`
condition in `rt_free_slab` because at least one chunk in the same
slab is held by a long-lived actor (handle or main).  The held chunk
prevents the rest of the slab from munmap'ing on death.

To prove: instrument `rt_allocate_raw` / `rt_free` with a per-class
counter; after 10 k req compare totals.  The class with non-zero net
== leak source.

## Workaround

Until root cause is fixed: use default bucket allocator (omit
`LAKE_SLAB_ALLOC=1`).  Bucket has the same overall budget but no
slab-pinning effect (no munmap-on-empty to be blocked).
