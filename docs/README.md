# Lake compiler internals

This directory documents the **how** of the Lake compiler — invariants
and abstractions that aren't obvious from reading the source linearly.

## Reading order

For a newcomer (or any of us after a memory wipe):

1. **[architecture.md](architecture.md)** — the actor model, CPS execution,
   what a "machine" is, why everything compiles to a state-dispatch shape.
2. **[pipeline.md](pipeline.md)** — end-to-end stages from `.lake` source to
   ELF: lex → parse → populate → lower → re-populate → resolve → typeck →
   codegen → link.
3. **[lowering.md](lowering.md)** — the 7-phase ret-machine desugar.
   This is the most subtle pass in the whole compiler and the place most
   bugs live.
4. **[runtime.md](runtime.md)** — scheduler main loop, ExecCtx / ProcessCtx
   / ShedulerCtx layouts, stop codes, mailbox protocol.
5. **[memory.md](memory.md)** — `rt_allocate` bucket allocator, free-list
   pop+zero, fat-pointers, what's leaked vs reclaimed.
6. **[concurrency.md](concurrency.md)** — quantum, reduction counting,
   fairness guarantees, cooperative yield boundaries, why
   `let r = f(args)` is fundamentally an actor spawn.
7. **[types.md](types.md)** — `i64`, `str`, `buf`, `pid`, `atom`, `bool`,
   tuples, and how typeck reasons about compatibility.
8. **[ast.md](ast.md)** — short reference for `Expr`, `Item`, `Pattern`,
   `Branch` variants and their meaning.
9. **[glossary.md](glossary.md)** — terms that recur and tend to confuse
   (ret-machine vs spawn-style machine, CPS block vs Cranelift block, etc).

## Existing notes

- [ideas.md](ideas.md) — design sketches not yet promoted to tasks.
- [io_uring_design.md](io_uring_design.md) — async I/O strategy.
- [notes/](notes/) — older working notes.
- [todo.md](todo.md) — informal tracker (mostly superseded by `cklog`
  task list).
