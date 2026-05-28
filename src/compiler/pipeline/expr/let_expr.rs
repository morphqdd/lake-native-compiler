use anyhow::Result;
use cranelift::{
    codegen::ir::{Block, BlockArg},
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};
use lake_frontend::api::{ast::Type, expr::Expr};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, compile_expr, pure_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile `let ident: ty [= default]`.
///
/// If `default` is present, it is compiled first (which stores its result in
/// `TEMP_VAL`). Then we open a new block that reads `TEMP_VAL` and writes it
/// into the variables array at the slot assigned to `ident`.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    ident: &str,
    ty: &Type<'_>,
    default: Option<&Expr<'_>>,
    entry: Option<Block>,
    fall_through: Option<Block>,
    omit_exit: bool,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    // ── #80 Level 1: pure-default coalesce ────────────────────────────────────
    // `let x = <pure>` burns two CPS blocks: pure_expr::compile emits
    // one block that stashes the value in TEMP_VAL and dispatches to
    // qb(block_id+1), then let's own block reads TEMP_VAL and writes
    // it to vars[slot].  The TEMP_VAL round-trip is dead weight — we
    // fold the pure expression inline and store the resulting Cranelift
    // Value directly into the variables slot in a single block.
    //
    // #80 Level 2 (slow path): when default is non-pure, the inner
    // sub-blocks still go through qb (one per arg / sub-expr), but the
    // FINAL save block can fall_through to the next statement, skipping
    // a qb dispatch.  Saves ~5 ns per let-with-call.
    if let Some(d) = default {
        if pure_expr::is_pure(d) {
            return compile_pure_let(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                ident,
                ty,
                d,
                entry,
                fall_through,
                omit_exit,
            );
        }
    }

    // Slow path: non-pure default.  This path cannot accept `entry`
    // or `omit_exit` because the default's compile_expr will create
    // its own first block — branch.rs is responsible for routing
    // fast-path / super-block runs only through statements whose
    // `accepts_entry` predicate returns true (pure_expr / pure-let).
    debug_assert!(
        entry.is_none() && !omit_exit,
        "let_expr slow path cannot participate in super-block runs — branch.rs/wait_expr should only chain through pure / pure-let stmts"
    );

    let next_id = match default {
        Some(d) => match compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            d,
            None,
            None,
            false,
        )? {
            StmtOutcome::Continue(id) => id,
            // A terminal default is unusual but we propagate it.
            terminal => return Ok(terminal),
        },
        None => block_id,
    };

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Register the variable and get its slot index.  Composite types
    // (Struct/Tuple, atom, user-defined records) all live as fat-
    // pointer-sized i64s at runtime, so collapse them to "i64" before
    // the type-map lookup; the surface string is still kept as the
    // Lake-level type for downstream diagnostics via `lake_type_of`.
    // A `Type::Named` for any non-builtin (i.e. typeck-accepted record
    // name) falls into the same i64 slot since Lake's ABI is uniformly
    // pointer-sized for reference types.  #058 followup.
    let lookup_key = match ty {
        Type::Struct(_) | Type::Unit | Type::Unknown => "i64".to_string(),
        Type::Named(ident) if ident.inner.0 == "atom" => "i64".to_string(),
        Type::Named(ident) => {
            let n = ident.inner.0;
            if ctx.lookup_type(n).is_some() {
                n.to_string()
            } else {
                "i64".to_string()
            }
        }
        _ => ty.to_string(),
    };
    let cranelift_ty = ctx
        .lookup_type(&lookup_key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown type '{}'", ty.to_string()))?
        .unwrap_simple();
    let var_index = state.insert_with_lake_type(ident.to_string(), cranelift_ty, ty.to_string());

    // #81 — Inline rt_load_u64 / rt_store for TEMP_VAL → vars[var_index].
    // Scheduler-trusted memory.
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let temp_val = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::TEMP_VAL,
    );
    let vars_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::VARIABLES,
    );
    let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);
    builder.ins().store(
        MemFlags::trusted(),
        temp_val,
        vars_start,
        var_index as i32 * 8,
    );
    let _ = rt_funcs;

    // #80 Level 2: slow path exit — fall_through if eligible, else qb.
    // The internal sub-blocks (default compilation) still go through qb;
    // only the FINAL save block can fall_through to the next statement.
    pure_expr::emit_continue(builder, ctx, ptr_ty, next_id, fall_through);

    branch_switch.set_entry(next_id as u128, b);
    Ok(StmtOutcome::Continue(next_id + 1))
}

/// Fast path for `let x = <pure>` (#80 Level 1 — CPS block coalescing).
///
/// Without this, the generic path goes through `pure_expr::compile`
/// (one block, stores to TEMP_VAL, jumps qb(block_id+1)) and then a
/// second block that reads TEMP_VAL and stores to vars[slot] before
/// dispatching to qb(block_id+2).  The TEMP_VAL round-trip burns a
/// full CPS block boundary (2x dispatch cost) for no real reason.
///
/// Here we fold the pure expression's Cranelift Value directly inside
/// the let's block and store it straight into vars[slot] — one block,
/// one quantum tick, identical observable behaviour.
///
/// Pre-conditions: caller has verified `pure_expr::is_pure(default)`.
fn compile_pure_let(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    ident: &str,
    ty: &Type<'_>,
    default: &Expr<'_>,
    entry: Option<Block>,
    fall_through: Option<Block>,
    omit_exit: bool,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // #80 Level 2: caller-supplied entry block (chained from prev stmt's
    // fall_through), else create our own.
    let b = match entry {
        Some(e) => {
            builder.switch_to_block(e);
            e
        }
        None => {
            let b = builder.create_block();
            builder.switch_to_block(b);
            b
        }
    };

    // Load exec_start (cached on the machine but we still need to
    // reach the variables pointer + own_pid through it).
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    // Mirror pure_expr::compile: load vars_start only if expression
    // reads at least one Var (otherwise the load is dead and litters
    // CLIF), load own_pid only if expression mentions `self`.
    let vars_start = if pure_expr_uses_vars(default) {
        let vars_fp = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        Some(builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0))
    } else {
        None
    };
    let self_pid = if pure_expr_uses_self(default) {
        Some(builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::OWN_PID,
        ))
    } else {
        None
    };

    // We need `state` borrowed mutably for insert, but `fold_with_self`
    // needs an immutable borrow.  Insert FIRST, then fold using the
    // updated state.  Note: `default` cannot reference `ident` itself
    // (let-binding shadowing happens AFTER the RHS evaluates), so a
    // pre-insert that adds `ident` to the table is safe — the fold
    // will only resolve names that already exist in `default`'s scope.
    //
    // Composite types (Struct/Tuple, atom) live as fat-ptr i64s at
    // runtime — collapse for the type-map lookup, same as the slow path.
    let lookup_key = match ty {
        Type::Struct(_) | Type::Unit | Type::Unknown => "i64".to_string(),
        Type::Named(ident) if ident.inner.0 == "atom" => "i64".to_string(),
        Type::Named(ident) => {
            let n = ident.inner.0;
            if ctx.lookup_type(n).is_some() {
                n.to_string()
            } else {
                // Unknown name reaches codegen only after typeck
                // accepted it — must be a user-defined record.
                // ABI is uniformly pointer-sized.  #058 followup.
                "i64".to_string()
            }
        }
        _ => ty.to_string(),
    };
    let cranelift_ty = ctx
        .lookup_type(&lookup_key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown type '{}'", ty.to_string()))?
        .unwrap_simple();

    // Fold BEFORE inserting `ident` into state so a self-reference in
    // the RHS (which would be a typeck error anyway) doesn't silently
    // resolve to the not-yet-initialized slot.
    let val = pure_expr::fold_with_self(default, builder, ptr_ty, vars_start, self_pid, state);

    let var_index = state.insert_with_lake_type(ident.to_string(), cranelift_ty, ty.to_string());

    // #81 — Inline rt_store for val → vars[var_index] (scheduler-trusted).
    // Use the exec_start we already loaded above when computing
    // vars_start for fold (avoid double-loading the fat-ptr deref chain).
    let vars_target = match vars_start {
        Some(s) => s,
        None => {
            // The folded expr didn't read any vars, so we never loaded
            // vars_start.  Load it now.
            let ctx_ptr = builder.use_var(machine_ctx_var);
            let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
            let vars_fat = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                exec_start,
                ExecCtxLayout::VARIABLES,
            );
            builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0)
        }
    };
    builder
        .ins()
        .store(MemFlags::trusted(), val, vars_target, var_index as i32 * 8);

    // #80 Level 2/3: emit exit unless we're mid-super-block.
    if !omit_exit {
        pure_expr::emit_continue(builder, ctx, ptr_ty, block_id, fall_through);
    }

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(block_id + 1))
}

/// Mirror of `pure_expr::has_var` — but that fn is private to its
/// module.  Replicate the small predicate here so we don't have to
/// widen pure_expr's API surface for one caller.  Keep in sync with
/// pure_expr.rs if pure expression shape ever grows new variants.
fn pure_expr_uses_vars(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => false,
        Expr::Var(..) => true,
        Expr::Neg(inner) => pure_expr_uses_vars(&inner.inner),
        Expr::TupleIndex { receiver, .. } => pure_expr_uses_vars(&receiver.inner),
        Expr::Index { receiver, index } => {
            pure_expr_uses_vars(&receiver.inner) || pure_expr_uses_vars(&index.inner)
        }
        Expr::Jump { args, .. } => args.iter().any(|a| pure_expr_uses_vars(&a.inner)),
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r)
        | Expr::BAnd(l, r)
        | Expr::BOr(l, r)
        | Expr::BXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => pure_expr_uses_vars(&l.inner) || pure_expr_uses_vars(&r.inner),
        _ => false,
    }
}

fn pure_expr_uses_self(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => true,
        Expr::Neg(inner) => pure_expr_uses_self(&inner.inner),
        Expr::TupleIndex { receiver, .. } => pure_expr_uses_self(&receiver.inner),
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r)
        | Expr::BAnd(l, r)
        | Expr::BOr(l, r)
        | Expr::BXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => pure_expr_uses_self(&l.inner) || pure_expr_uses_self(&r.inner),
        _ => false,
    }
}
