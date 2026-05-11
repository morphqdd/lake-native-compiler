# AST reference

Quick reference for the variants in `lake-frontend/src/api/`.

## Item

```rust
pub enum Item<'src> {
    Machine(Spanned<Machine<'src>>),
    Import(Import<'src>),
    Directive(Directive<'src>),
    Const(Spanned<Const<'src>>),
}
```

A `.lake` file's top level is a `Vec<Spanned<Item>>`.

- **Machine** — `name is { branches... }`.  See below.
- **Import** — `+module.path` or `+module.path.{ item1 item2 }` or
  `+module.path as alias`.
- **Directive** — `@rt(name)` / `@ffi(...)` / etc.  Attached metadata
  for declarations.
- **Const** — `const NAME = <literal>` compile-time constant.

## Machine

```rust
pub struct Machine<'src> {
    pub ident: Spanned<Ident<'src>>,
    pub vis: bool,                      // true if `pub`
    pub generics: Vec<...>,             // not used yet
    pub items: Vec<Spanned<MachineItem<'src>>>,
}

pub enum MachineItem<'src> {
    Branch(Branch<'src>),
    // ... fields, etc. in future
}
```

## Branch

```rust
pub struct Branch<'src> {
    pub label: Option<Spanned<Ident<'src>>>,    // `@init` etc.
    pub patterns: Vec<Spanned<Pattern<'src>>>,  // parameters
    pub ret_ty: Option<Spanned<Type<'src>>>,    // `-> ret <ty>` annotation
    pub body: Vec<Spanned<Expr<'src>>>,
}
```

`patterns` are the parameter list.  `ret_ty.is_some()` marks this as a
ret-machine branch.

## Pattern

```rust
pub struct Pattern<'src> {
    pub ident: Spanned<Ident<'src>>,
    pub ty: Spanned<Type<'src>>,
    pub kind: PatternKind,              // Var | Wildcard | NumGuard(i64) | StrGuard
}
```

- `Var` — normal binding: `n i64`.
- `Wildcard` — `_` matches anything, no binding.
- `NumGuard(n)` — `0 i64`, `42 i64` — literal value guard.
- `StrGuard` — `"hello" str` — literal string guard.

Guards participate in branch dispatch: a call with arg value matching
the guard prefers that branch over a wildcard.

## Type

```rust
pub enum Type<'src> {
    Named(Spanned<Ident<'src>>),               // i64, str, buf, pid, bool, atom
    Generic(Spanned<Ident<'src>>, Vec<...>),   // box(T) — not yet implemented
    Path(Spanned<Ident<'src>>, ...),           // core:io:writer
    Unit,                                       // {}
    Struct(Vec<Spanned<Type<'src>>>),          // anonymous tuple type
    Unknown,                                    // resolver placeholder
}
```

## Expr (the main AST node)

```rust
pub enum Expr<'src> {
    // Literals
    Num(&'src str, Type<'src>),
    String(&'src str, Type<'src>),
    Bool(bool),
    Unit,                                     // {}
    Atom(&'src str),                           // :ok

    // Variables & paths
    Var(&'src str, Type<'src>),
    Path(Vec<Spanned<Ident<'src>>>),          // core:io:writer

    // Binding
    Let { ident, ty, default: Option<Box<...>> },
    LetTuple { fields, default: Box<...> },    // let { a b c } = expr

    // Calls & access
    Jump { ident: Box<Expr>, args: Vec<Expr> }, // f(args) — main call shape
    MethodCall { receiver, method, args },     // receiver@method(args)
    AtAccess { receiver, field },              // receiver@field
    DotAccess { receiver, field },             // receiver.field
    StructInit { base, fields },               // base.{ fields }

    // Composite
    Tuple(Vec<Spanned<Self>>),                 // { a b c }
    TupleIndex { receiver, index: usize },     // t.0
    Index { receiver, index: Box<Self> },      // buf[i]

    // Control flow
    When { cond, branches },                   // pattern matching
    Wait { handlers, filter },                 // mailbox receive
    Ret(Box<...>),                             // ret expr
    Pin(Box<...>),                             // pin expr — sync sugar

    // Arithmetic / bitwise / cmp
    Add(L, R), Sub(L, R), Mul(L, R), Div(L, R), Neg(_),
    Eq(L, R), Lt(L, R), Gt(L, R), Le(L, R), Ge(L, R),
    BAnd(L, R), BOr(L, R), BXor(L, R), Shl(L, R), Shr(L, R),
}
```

### Notes on key variants

- **Jump** — the universal call shape.  `f(args)`, `self(args)`,
  `pid -> msg`, `M(args)` all become `Expr::Jump`.  The runtime
  distinguishes via callee's resolved type.
- **Ret** — only meaningful in ret-machine branches.  Lowering pass
  rewrites `Ret(x)` into a send-to-`__caller`; codegen never sees
  `Expr::Ret` directly (post-lowering).
- **Pin** — sync sugar.  `pin f(args)` → `let __pin = f(args)`,
  ensuring synchronous execution.  Lowered away in Phase 1.b.
- **Index** — `buf[i]` postfix.  Backend emits `rt_load_u8` call
  internally; no user-visible `rt_load_u8` requirement.
- **TupleIndex** — `.N` numeric access.  Backend computes
  `load(fat_ptr.start + index * 8)`.

## Token

`lake-frontend/src/api/token/mod.rs`.  Output of the lexer.  Notable
distinction between adjacency-tagged variants:

- `Parens` — `(...)` with whitespace before `(`.  Grammar reads as
  paren-group (value position).
- `TightParens` — `(...)` adjacent to previous token.  Grammar reads as
  call.
- `SquareBrackets` — `[...]` with whitespace.  Grammar reads as filter
  list (used in `wait [pid] { ... }`).
- `TightSquareBrackets` — `[...]` adjacent.  Grammar reads as index
  (`buf[i]`).

This whitespace-significance is the main novel-ish lexical rule.
