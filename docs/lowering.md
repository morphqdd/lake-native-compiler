# Lowering — the ret-machine desugar

`lake-frontend/src/lowering.rs` (~1700 lines).  This is the most subtle
pass in the compiler.  Bugs here surface as runtime hangs, wrong values,
or "machine wasn't lowered" panics later in typeck.

## The fundamental rewrite

A ret-machine call `let r = f(args)` is **semantically** "spawn an actor,
wait for it to reply with a value".  Source code hides this; lowering
makes it explicit by rewriting to:

```
let __ret_N_pid pid = f(self, args)          # spawn (Jump with caller pid)
wait __ret_N_pid {                            # yield + wait for reply
  __ret_N_sender pid r <ret_ty> ->
    { ...rest of the body... }                # bound r in continuation
}
```

The ret-machine's body is similarly transformed: every `Expr::Ret(x)`
becomes a send back to the caller's pid (stored in the prepended
`__caller` parameter).

## Why two sides

- **Callee side** (the ret-machine itself): each branch gets `__caller pid`
  prepended.  `ret x` becomes `Jump { __caller, [self, x] }` — a 2-arg
  message send carrying (sender, value).
- **Caller side** (the call site): the spawn produces a pid; the wait
  receives the reply and binds the value name.

Both sides must agree on the protocol, hence both are touched in one
pass.

## Idempotency

Lowering runs **once**, but the result needs to round-trip through a
second `populate_from` (so the registry sees the new __caller params)
without re-applying.  Idempotency checks:

- `lower_branch`: skip prepending __caller if the first pattern is
  already named `__caller`.
- `thread_caller_in_self_calls`: skip if the first arg of `self(...)`
  is already `Var("__caller", _)`.
- `wrap_bare_ret_call_in_let`: skip if the call's first arg is a pid
  expression (sentinel meaning "already wrapped on a prior pass").
- Phase 3 (tail wrap): only fires when `is_outermost == true`.

## The seven phases

`lower_body_impl` runs phases in order.  Each is a list-of-expr →
list-of-expr transform.  Order matters.

### Phase -1: `flatten_paths`

`io:println(s)` → `println(s)`.  Backend's `jump_expr` only resolves by
flat name, so module-qualified paths collapse here once the resolver has
confirmed the target exists.

### Phase 0a: `inline_consts`

Replace `Expr::Var(name, _)` whose binding resolves to `Resolution::Const`
with the literal expression encoded in the const.  After this, the rest
of the pipeline never sees const references.

### Phase 0b: `expand_let_tuple`

`let { a b c } = expr` → synthetic `__dst_<id>` for the source plus one
`let a = __dst_<id>.0; let b = __dst_<id>.1; ...` per field.

### Phase 1.0: `thread_caller_in_self_calls`

Inside a ret-branch, `self(x y)` becomes `self(__caller x y)`.  Without
this, the recursive tail call would have one fewer arg than the
post-prepend signature.  Idempotent via `already_threaded` check.

### Phase 1: `rewrite_ret_in_expr`

`Expr::Ret(x)` → `Jump { __caller, [self_pid, x] }`.  Recursive across
the expression tree (so a Ret inside a When arm body is caught).

### Phase 1.5: `lift_nested_calls`

`f(g(x))` → `let __lift_<id> = g(x); f(__lift_<id>)`.

Why: `g(x)` is a ret-machine call which becomes spawn+wait, and you
can't have spawn+wait in argument position — the wait yields control,
the outer call would never see the result.  Pre-lifting flattens the
shape so Phase 2 can rewrite each call linearly.

Lift recurses through:
- `Add`, `Sub`, `Mul`, `Div`, `Shl`, `Shr`, `BAnd`, `BOr`, `BXor` —
  binary operators with potential ret-call operands.
- `Eq`, `Lt`, `Gt`, `Le`, `Ge` — comparison operators.
- `Neg` — unary minus.
- `When` discriminant, `Wait` filter expressions, `Pin` body — these all
  have control-flow shape but a single computed value position.

After lift, every ret-call lives in one of: `let x = call`, statement
position, `__caller(self, ...)` send.

### Phase 1.b: `rewrite_pin`

`pin <expr>` → `let __pin_<id> = <expr>` so the existing let-with-ret
sweep catches it.  For non-ret callees, the synthetic let just binds an
unused name (harmless).

### Phase 1.c: `wrap_bare_ret_call_in_let`

`M(args)` standalone → `let __discard_<id> = M(args)`.  Without this, a
bare ret-machine call would race: the body races ahead while the side
effects of the callee (writes to shared heap state) are still pending.
SHA-256's inner loop wouldn't terminate correctly without this guard.

### Phase 2: `expand_let_with_ret`

The headline transform.  `let r = M(args)` becomes:

```
let __ret_N_pid pid = M(self, args)
wait __ret_N_pid {
  __ret_N_sender pid r <ret_ty> -> { ...rest of body... }
}
```

`rest of body` is everything after the let in the enclosing list,
recursively lowered as a new body so its own ret-calls expand similarly.

The wait filter `[pid_expr...]` restricts dispatch to messages from
the spawned pid — a concurrent message from a different sender can't
satisfy the receive.

### Phase 3: `wrap_in_caller_send` (tail wrap)

If the body of a ret-branch falls off the end without an explicit `ret`,
its last expression's value is the implicit return.  Phase 3 wraps that
tail in `Jump { __caller, [self, <tail>] }` so the value flows back.

Only fires on the **outermost** body (the branch's top-level body), not
recursively into arm bodies — those get separate handling via Phase 4.

### Phase 4: `lower_arm_bodies_in_expr`

Re-enter `When` arms and `Wait` handler bodies, lowering each as its own
body pass with `is_outermost = false` (so Phase 3 doesn't double-wrap).

Without Phase 4, a `let r = M(args)` written inside a When arm stays
unlowered — typeck then fails because the body looks like an unknown
call.

## Pure ret-machine detection (#78 phase 1)

`collect_pure_ret_machines` walks all ret-machines and tags those whose
bodies don't yield the scheduler.  Definition of "pure":

- No `wait`, no `pin`, no `Expr::Ret` in non-tail position.
- No `self()` recursion.
- No calls to non-pure machines.
- No I/O rt-calls (only allowed rt: rt_allocate / rt_free / rt_store /
  rt_load_u* / rt_copy_bytes / rt_mmap / rt_munmap).

The set is computed via fixpoint over the call graph and currently used
only for diagnostics (`LAKE_DUMP_PURE=1`).  Phase 2/3 of #78 will use it
to skip the spawn+wait rewrite for calls to pure machines and emit a
direct Cranelift `call` instead.

## Bug-prone areas

Things that have broken in this pass before:

1. **`type_from_string`** — a whitelist of "primitive type names" the
   lowering knows how to materialise.  Missing entries silently become
   `Type::Unknown` and downstream inference falls apart.  When adding a
   new primitive type (e.g. `buf`), update this whitelist along with
   resolver's `type_from_str`.
2. **Phase 4 idempotency** — Phase 1.0 needs `already_threaded` guard,
   Phase 1.c needs `first_arg_is_pid` check, Phase 3 needs
   `is_outermost`.  Forgetting one causes double-wrap → wrong arity.
3. **Bare ret-call sequencing** — Phase 1.c is essential for side-effect
   ordering inside hot loops.
4. **Lift recursion through binary ops** — easy to add a new operator
   and forget to extend `lift_arg`.  Result: nested calls in
   `(a << b) | c` don't get lifted, Phase 2 sees a call inside Jump
   args, downstream typeck breaks.
