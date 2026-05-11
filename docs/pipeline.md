# Compilation pipeline

End-to-end stages from `.lake` source to ELF binary.

## Entry point

`lake-native-compiler/src/main.rs` → `build_program` in
`lake-frontend/src/prelude/mod.rs`.

```
ProgramSources              (FS load: entry + +imports, dedupe, cycle check)
  ↓ parse_all                (chumsky lexer + parser, per-file)
ParsedProgram               (Vec<ParsedModule { module_path, ast }>)
  ↓ registry.populate_from   (first pass: machines, FFI, consts, imports)
ProgramRegistry             (rt_fns + per-module ModuleScope)
  ↓ lower_program            (ret-machine desugar — see lowering.md)
ParsedProgram (lowered)     (mailbox round-trips made explicit)
  ↓ registry.populate_from   (second pass: now picks up __caller params)
ProgramRegistry (refreshed)
  ↓ resolve_program          (bind variable types, walk references)
ParsedProgram (resolved)
  ↓ typecheck_program        (signature dispatch, arg-shape errors)
LakeProgram                  (resolved AST + registry)
  ↓ codegen (lakec)           (Cranelift IR → object → linker)
ELF binary
```

## Lexer

`lake-frontend/src/lexer/mod.rs`

- Chumsky-based, character-by-character.
- Produces `Vec<Spanned<Token>>` flat at top level, with nested token
  groups inside `Parens` / `SquareBrackets` / `CurlyBrackets` variants.
- Post-pass `tag_paren_adjacency`: any `(` / `[` whose byte-start equals
  the previous token's byte-end gets the `Tight*` variant.  This is how
  `f(x)` (call) is distinguished from `f (x)` (Var + paren-group): the
  former produces `Token::TightParens`, the latter `Token::Parens`.
  Same trick for `buf[i]` postfix indexing.

## Parser

`lake-frontend/src/parser/`

- Chumsky pratt parser over the token stream.
- Top-level: `Item` = `Machine | Import | Directive | Const`.
- Expression precedence (low → high):  `=` (let RHS) → `==` → `<`/`>`/`<=`/`>=`
  → `|` → `^` → `&` → `<<`/`>>` → `+`/`-` → `*`/`/` → prefix `-` → postfix
  `()` `[]` `@` `.`.
- Whitespace-significant calls: `f(x)` is a call only when `(` is
  `TightParens` (no whitespace between callee and parens).
- Comments are filtered at lex post-pass.

## populate_from (first pass)

`lake-frontend/src/registry.rs`

Walks every parsed module and inserts machines, FFIs, consts, imports
into the `ProgramRegistry`.  Per machine, builds a `MachineEntry` whose
`branches: Vec<Signature>` carries one entry per AST branch.

After this first pass, ret-machines have signatures with the
*user-visible* parameter list — no `__caller` yet (the lowering pass
hasn't run).

## lower_program

`lake-frontend/src/lowering.rs`

The seven-phase desugar described in [lowering.md](lowering.md).  Two
crucial transformations:

- **Callee side**: ret-machine branches get `__caller pid` prepended;
  `Expr::Ret(x)` becomes a send to that caller.
- **Caller side**: `let r = M(args)` becomes `let pid = M(self, args); wait
  pid { ... r ty -> rest }`.

## populate_from (second pass)

Re-walk after lowering so the registry's branch signatures now begin
with `pid` (the `__caller` param).  Without this re-pop, typeck would
look up the old (pre-lowering) signature and reject the desugared call.

## resolve_program

`lake-frontend/src/resolver/mod.rs`

Three jobs:

1. **Type inference** for `let` RHS — looks up call-target return types
   via the registry, plus inference rules for arithmetic / literals.
2. **Variable resolution** — binds each `Expr::Var(name, Unknown)` to the
   type from the enclosing scope (branch patterns + let bindings).
3. **Wait handler scoping** — wait-handler patterns introduce bindings
   visible inside the handler body.

## typecheck_program

`lake-frontend/src/typeck/mod.rs`

For each `Expr::Jump { callee, args }`:

1. Resolve the callee (rt fn, machine, ffi, import).
2. For machines: match arity, then for each branch check
   `types_compatible(param_ty, arg_ty)` pairwise.
3. On no match: emit E003 "no branch matches call".

Compatibility rules (`types_compatible`):

- `param == arg` → ok.
- `(buf, str)` / `(str, buf)` → ok (same fat-ptr layout, mutability
  enforced later by #76).
- All other mixes → reject.

## Codegen (lakec)

`lake-native-compiler/src/compiler/mod.rs` → per-machine compile via
`pipeline::machine::compile_machine`.

Compiled machine signature: `fn(ctx_fat_ptr: ptr) -> stop_code: ptr`.

Body structure (every machine, see machine.rs):

```
entry
  └─ machine_switch_block       ← dispatch by BRANCH_ID
       └─ branch_switch_block   ← dispatch by BLOCK_ID inside this branch
            └─ <user code as CPS blocks ending with `jump quantum_continue`>
                 └─ quantum_continue_block: check stop_code, decrement,
                                             re-enter or yield
```

Each user expression `e` compiles to a Cranelift block via
`pipeline::expr::compile_expr`, ending with
`jump quantum_continue, [BlockArg::Value(next_id)]`.

The scheduler is emitted once, by `rt::scheduler::build_scheduler`,
calling each machine's compiled fn via `call_indirect`.

## Link

Object file (`build/program.o`) is linked together with the embedded
`syscall.o` (raw syscalls — no libc) by `mold` (default) or `lld`,
producing the final executable.
