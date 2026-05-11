# Glossary

Terms that recur in the codebase and tend to confuse.  Disambiguate
before reading a new file.

## Machine

A Lake top-level function-like declaration.  Two kinds:

- **Spawn-style machine** — no `-> ret <type>`.  Calling by name
  **starts a new actor**.  Does not return a value to the caller.
  Example: `counter is { n i64 -> { ... } }`.
- **Ret-machine** — has `-> ret <type>`.  Calling returns a value, but
  mechanically it's still actor spawn + mailbox roundtrip.  Example:
  `rotr32 is { x i64 n i64 -> ret i64 { ... } }`.

A "function call" in C terms doesn't exist in Lake yet — every call is
some flavor of actor.

## Actor

A machine **in execution**.  Has its own ExecCtx, mailbox, pid.

## Branch

A single `pattern1 ... patternN -> { body }` clause within a machine.
A machine can have multiple branches that dispatch based on argument
shape (arity + literal guards + types).

The current SHA-256 helpers each have exactly one branch — multi-branch
dispatch is rarer.

## CPS block

A Cranelift basic block that ends with `jump(quantum_continue, next_id)`
instead of falling through.  Every Lake expression compiles to one CPS
block.  Quantum + reduction counting drive scheduler fairness via these.

**Not** the same as a Cranelift block in general — Cranelift uses
blocks for many internal control-flow purposes (entry, merge,
dispatch).  CPS block is specifically the per-expression body block.

## Quantum

The cap on how many CPS blocks a machine can execute per scheduler
tick.  Currently 256.  When exhausted, machine returns STOP_LIMIT.

## Reduction

One unit of CPS execution.  In BEAM terminology.  A Lake actor's
"reduction count" is `quantum_initial - quantum_remaining` after a tick.

## Pid

Process id.  In Lake: a 64-bit value where the low 32 bits are the slot
index into the scheduler's `pid_table` and the high 32 bits are a
generation tag.  Generation is incremented on slot reuse so that a
stale pid (held by an actor that doesn't know its target died) sends
to a freshly-recycled slot fail the gen check and drop silently.

## ExecCtx

Per-actor execution state (72 bytes).  Branch id, block id, scratch,
variables, jump args, mailbox.  See [runtime.md](runtime.md).

## ProcessCtx

Pairs a function pointer with an ExecCtx (24 bytes).  Stored in the
scheduler's process_arr.

## ShedulerCtx

The single global scheduler state (224 bytes).  Process queues, io_uring
ring pointers, pid table.

## VARIABLES

The flat 8-byte-slot array storing local bindings (let + branch params)
for one actor.  Indexed by slot position.  Backed by a fat-ptr in
ExecCtx at offset 24.

## JUMP_ARGS

The transient array used to stage arguments for the next call (`f(a b
c)` writes `a`, `b`, `c` into successive jump_args slots).  Spawner
copies jump_args → callee's VARIABLES.  Fat-ptr in ExecCtx at offset 32.

## TEMP_VAL

Scratch slot in ExecCtx (offset 16).  Used to pass intermediate values
between CPS blocks of the same actor (e.g. `let x = rt_load_u8(buf
off)` stores result in TEMP_VAL before the next block reads it into a
VARIABLES slot).

## Stop code

Return value of a compiled machine fn.  Tells the scheduler what to do
next:
- STOP_DONE (-1) — actor finished, reclaim memory
- STOP_LIMIT (-2) — quantum used up, round-robin
- STOP_WAIT (-3) — actor blocked on wait, move to wait_arr
- STOP_PARK (-4) — actor parked on io_uring, slot already vacated

## Fat-ptr

16-byte struct `{ start: i64, end: i64 }`.  Every "heap-allocated
buffer" reference in Lake is a pointer to a fat-ptr struct.  Bounds
checks read `end` before deref'ing.

## `__caller`

Synthetic first parameter prepended to every ret-machine branch by
lowering Phase 1.0.  Holds the caller's pid so the `ret expr`
rewrite can send the value back.

## `__ret_N_pid`, `__ret_N_sender`

Synthetic names introduced by Phase 2 (let-with-ret expansion).
`__ret_N_pid` is the captured pid of the spawned child;
`__ret_N_sender` is the bound name for the sender pid in the wait
handler (always equal to `__ret_N_pid` thanks to the filter).

## `__lift_N`

Synthetic name introduced by Phase 1.5 (lift_nested_calls).  Holds the
result of a previously-nested ret-machine call so the outer call can
read it as a normal Var.

## `__pin_N`

Synthetic name from Phase 1.b (rewrite_pin).  Effectively a discarded
binding for `pin expr` — used only as a vehicle for routing through the
let-with-ret expansion machinery.

## `__discard_N`

Synthetic name from Phase 1.c (wrap_bare_ret_call_in_let).  A bare
ret-call needs await semantics; wrapping in a discarded let lets
Phase 2 see a let-with-ret-target and rewrite to spawn+wait.

## `__pat_N`

Synthetic key in BranchState for wildcard / literal-guard pattern slots
that don't bind a user-visible name.  Keeps positional indexing
consistent across branches with mixed Var/Wildcard patterns.

## CPS

Continuation-passing style.  Each expression becomes a "block that
takes a continuation"; jumping with `next_id` is the continuation
parameter.  Lake's CPS implementation is implicit — there's no
first-class continuation, but the block-by-block dispatch implements
the same execution model.

## ANF

Administrative normal form.  The convention that every intermediate
result has a name.  Lake's lowering achieves this (via Phase 1.5
lift_nested_calls) so the backend can compile each call independently.

## Pure ret-machine (#78)

A ret-machine whose body has no scheduler interaction: no wait, no
pin, no self() recursion, no spawn-style call, no I/O.  Calls to other
pure ret-machines and to pure rt-functions (rt_load_u*, rt_store,
rt_allocate, etc.) are allowed.  Eligible for inline compilation
(future phase 2/3 of #78) to skip the spawn+wait mailbox roundtrip.

## Pure rt-function

Subset of rt-functions that don't yield the scheduler and don't do
I/O.  Currently: rt_allocate, rt_free, rt_store, rt_load_u8/16/32/64,
rt_copy_bytes, rt_mmap, rt_munmap.  Calling these from a pure
ret-machine body keeps the body pure.

## Loader

`lake-frontend/src/loader.rs`.  Walks the entry file's `+import`
declarations, recursively loads all transitively-needed modules,
detects cycles, dedupes.  Returns `ProgramSources`.
