# Lake TODO

## 1. Allocator + scheduler structures
- [ ] Replace bump allocator with proper allocator (`HEAP_SIZE = 16 MiB` cap)
- [ ] Free / reuse memory for dead processes
- [ ] **Grow `process_arr` from static 256 slots to dynamic** —
  `rt/layout/sheduler_ctx.rs:28` hard-caps concurrent pending processes at
  256. Burst-spawning >256 actors in one quantum overflows the queue
  (caught by `benchmark/perf/spawn`).  Same applies to `wait_arr` (line 30).
- [ ] Benchmark memory usage

## 2. io_uring
- [ ] io_uring setup (sq/cq rings, syscall integration)
- [ ] Async write via io_uring (replace direct write syscall)
- [ ] Async read support
- [ ] Integration with cooperative scheduler (suspend on io, resume on completion)
- [ ] Benchmark vs current direct syscall baseline

## 3. Frontend coverage
- [ ] Audit what frontend parses but compiler cannot compile
- [ ] Generics / generic types
- [ ] Type checking pass
- [ ] Error messages (compile-time, not panics)
- [ ] Standard library (io, box, result)

### Known frontend bugs (caught by `tests/integration/`)
- [ ] **Literal-guard branches lose param type in match signature.**
  `f is { 1 i64 -> ...; 2 i64 -> ... }` rejects `f(1)` with E003 — reports
  available branches as `_, _` instead of `(i64), (i64)`.
  Repro: `tests/integration/guards.rs::int_guard_single_match_no_wildcard`.
- [ ] **Unary minus not accepted on call args.**
  `f(-7)` errors P001. Parser treats `-` as binary op only.
  Repro: `tests/integration/guards.rs::int_guard_negative_value`.
- [ ] **String `when` over `let`-bound variable produces no output.**
  `let buf str = "lake"; when buf { "lake" -> ... }` — match arm never fires.
  Distinct from the `examples/when_string.lake` path (needs investigation).
  Repro: `tests/integration/when_expr.rs::when_string_match`.
- [ ] **`//` comments break the parser inside machine bodies.**
  `CLAUDE.md` claims `//` is supported but P001 fires when used inside a
  `main is { _ -> { ... } }` block.  Caught while writing
  `benchmark/semantic/mailbox_isolation/lake.lake`.

## 4. Compiler hot path
- [x] **String literal guards in branch dispatch.** Implemented as
  `dispatch::emit_str_guard_select` (MPHF + hash-verify + memcmp).
  `emit_guard_select` dispatches between int/str variants by guard kind.
  Wired into spawn_expr.rs, jump_expr.rs, change_state_expr.rs.
  Verified by `tests/integration/regression.rs::str_guard_branch_dispatch_playground_repro`.
- [ ] **Reduction counter in register (Cranelift Variable).**
  Currently quantum lives in `ProcessCtx`; every CPS block does load/sub/cmp/store.
  Hoist to `Variable`, spill only at yield boundaries.
- [ ] **Decrement only on back-edges.**
  Forward-progress blocks should not tick the counter. Tick on `self(...)`,
  recursive calls, and explicit loops only. BEAM-style reduction model.
