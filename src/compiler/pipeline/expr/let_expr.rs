use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
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
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    // ── #80 Level 1: pure-default coalesce ────────────────────────────────────
    // `let x = <pure>` currently burns two CPS blocks: pure_expr::compile
    // emits one block that stashes the value in TEMP_VAL and dispatches
    // to qb(block_id+1), then let's own block reads TEMP_VAL and writes
    // it to vars[slot].  The TEMP_VAL round-trip is dead weight — we can
    // fold the pure expression inline and store the resulting Cranelift
    // Value directly into the variables slot in a single block.
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
            );
        }
    }

    // Compile the initialiser (if any) first; it leaves its result in TEMP_VAL.
    let next_id = match default {
        Some(d) => match compile_expr(ctx, builder, machine_ctx_var, block_id, branch_switch, state, d)? {
            StmtOutcome::Continue(id) => id,
            // A terminal default is unusual but we propagate it.
            terminal => return Ok(terminal),
        },
        None => block_id,
    };

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Register the variable and get its slot index.  Composite types
    // (Struct/Tuple, atom) all live as fat-pointer i64s at runtime, so
    // collapse them to "i64" before the type-map lookup; the surface
    // string is still kept as the Lake-level type for downstream
    // diagnostics via `lake_type_of`.
    let lookup_key = match ty {
        Type::Struct(_) => "i64".to_string(),
        Type::Named(ident) if ident.inner.0 == "atom" => "i64".to_string(),
        _ => ty.to_string(),
    };
    let cranelift_ty = ctx
        .lookup_type(&lookup_key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown type '{}'", ty.to_string()))?
        .unwrap_simple();
    let var_index = state.insert_with_lake_type(ident.to_string(), cranelift_ty, ty.to_string());

    // Read TEMP_VAL (the initialiser result) and write it into vars[var_index].
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
    let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let temp_val = builder
        .ins()
        .call(load_u64_ref, &[ctx_ptr, temp_val_offset]);
    let temp_val = builder.inst_results(temp_val)[0];

    let vars_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let vars_ptr_call = builder.ins().call(load_u64_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(vars_ptr_call)[0];

    let var_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[vars_ptr, temp_val, size, var_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, next_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_block_id)]);

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
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Load exec_start (cached on the machine but we still need to
    // reach the variables pointer + own_pid through it).
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

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
        Type::Struct(_) => "i64".to_string(),
        Type::Named(ident) if ident.inner.0 == "atom" => "i64".to_string(),
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

    // Store directly into vars[var_index] — no TEMP_VAL detour.
    let rt_funcs = ctx.rt_funcs().clone();
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
    let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let vars_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let vars_ptr_call = builder.ins().call(load_u64_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(vars_ptr_call)[0];

    let var_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[vars_ptr, val, size, var_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_block_id)]);

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
        | Expr::Shr(l, r) => {
            pure_expr_uses_vars(&l.inner) || pure_expr_uses_vars(&r.inner)
        }
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
        | Expr::Shr(l, r) => {
            pure_expr_uses_self(&l.inner) || pure_expr_uses_self(&r.inner)
        }
        _ => false,
    }
}
