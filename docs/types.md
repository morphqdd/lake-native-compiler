# Types

Lake's type system is **nominal + flat**.  All primitives lower to the
same Cranelift type (`i64`); distinctions exist at the source language
layer for ergonomics and error catching.

## Primitive types

| Name   | Runtime shape          | Meaning                                                   |
|--------|------------------------|-----------------------------------------------------------|
| `i64`  | 64-bit integer         | Numbers, indices, bit patterns.  Default for arithmetic.  |
| `str`  | i64 (fat-ptr address)  | Immutable byte sequence — string literals, concat result. |
| `buf`  | i64 (fat-ptr address)  | Heap-allocated mutable byte buffer.  rt_allocate result.  |
| `pid`  | i64 (slot-tagged)      | Process id with generation tag for safe slot reuse.       |
| `bool` | i64 (0 / 1)            | True or false.  No coercion to int.                       |
| `atom` | i64 (interned symbol)  | `:ok`, `:err` etc. — compile-time-interned tag values.    |

All six map to Cranelift's `Type::int(64)`.  The distinction is **only**
in the frontend's typeck — the runtime cannot tell `str` from `buf`
from `pid` at the bit level.

## Compatibility

`typeck::types_compatible(param, arg)`:

```
param == arg                          → ok
(buf, str) or (str, buf)              → ok
otherwise                             → reject
```

The `buf ↔ str` compatibility is because both have the same fat-ptr
runtime shape.  At call sites this lets `rt_write(1 "hello" 5)` accept
a `str` literal where the rt registry types the param as `buf`.

The mutability distinction (`str` is immutable, `buf` is mutable) is
enforced statically by **#76 linear types** — once it lands, writing to
a `str`-typed value is a typeck error.  Currently both can flow into a
`buf` parameter; the runtime doesn't actually distinguish, but the
language semantics will tighten.

## `i64` vs buffer types

Before #45, `i64` was used for both numbers and "buffer addresses" since
they share runtime shape.  This let bugs slip:

```lake
process_block is { buf i64 off i64 h i64 -> ... rt_copy_bytes(w 0 buf off 64) ... }
hash_blocks is { buf i64 total i64 off i64 h i64 -> ... process_block(blk h) ... }
                                                                 ^^^^^^^
                          // blk = buf + off — arithmetic on a fat-ptr address!
                          // Works for off=0 (single block) by accident.
                          // Crashes on off=64 (second block) — SIGILL.
```

After #45, `rt_allocate` returns `buf`, and stdlib helpers type their
buffer parameters as `buf`.  Mixing `i64` arithmetic with a `buf`-typed
value now gets caught at typeck.

## Tuples

`{ a b c }` — anonymous heap-allocated tuple, positional access via
`.N`:

```lake
let t = { 1 "hello" :ok }
let n = t.0    # = 1
let s = t.1    # = "hello"
let tag = t.2  # = :ok
```

First element may be an atom for tagged-tuple use: `{ :ok 42 }`.  Pattern
matching:

```lake
when t {
  { :ok v } -> { ... }
  { :err msg } -> { ... }
}
```

Tuple destructure on `let`:

```lake
let { a b c } = some_tuple_returning_fn(x)
```

This is the `Expr::LetTuple` AST variant, expanded in lowering Phase 0b
into a synthetic temporary + N positional lets.

Tuples are runtime fat-ptrs to N × 8-byte arrays.  Stored same as `buf`.
Tuple type at the source level: `Type::Struct(Vec<Type>)`.

## Atoms

`:identifier` — compile-time-interned symbol.  Each unique atom literal
gets a stable i64 value.  Equality compares atom indices: `:ok == :ok`
is a fast integer compare, no string lookup at runtime.

Used as tags in tuples (`{ :ok value }`) and as match discriminators
(`when status { :running -> ... ; :stopped -> ... }`).

## Type::Unknown

The resolver's placeholder for "not yet determined".  Surfaces as `?`
in error messages.  When a `let r = f(args)` can't determine the
return type of `f`, `r`'s type becomes `Unknown`, and downstream typeck
will reject any call that uses `r` as an arg.

The fix is usually one of:
- Add the function to `rt_registry.rs`.
- Make sure the function is parsed/registered before the call site.
- For ret-machines: confirm the lowering's `branch_signature` reads the
  correct `ret_ty` (this was the #45 bug — `Resolution::Machine.return_type()`
  used to hardcode `"pid"` and ignore the actual ret type).

## Where types end up being checked

- **Resolver** (`resolver/mod.rs`): infers let-RHS type via call return
  lookup; binds pattern variables to their declared type.
- **Typeck** (`typeck/mod.rs`): for each Jump, looks up callee's branches
  and tries to match arg types to one branch.  Errors:
  - E001 — undeclared variable
  - E003 — no branch matches call (with available signatures shown)
  - E004 — no callable with this name in scope
- **Backend** (compiler/mod.rs / pipeline/): assumes typeck passed.
  Uses `canon_arg_ty` to collapse `str | atom | pid | buf` → `i64` for
  branch hashing.  This is the dispatch-time canonical type.
