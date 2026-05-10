use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, pure_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile an anonymous tuple literal `{ a b c ... }`.
///
/// Layout: heap-allocated payload of `N * 8` bytes, fat-ptr header
/// `{start, end}` produced by `rt_allocate`.  Element values are
/// folded inline within this block — each element must therefore be
/// a `pure_expr::is_pure` shape (Var, Num, Bool, Atom, arith, …).
/// Non-pure elements (e.g. `{ Foo(args) bar }`) are rejected here.
///
/// Result: the fat-ptr address (i64) is stashed into TEMP_VAL so the
/// next CPS block — typically a `let_expr` — can pick it up and bind
/// it into a variable slot.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    elems: &[Expr<'_>],
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // All elements must be pure so we can evaluate them eagerly within
    // a single block.  Non-pure shapes (Jump / When / Wait / Let) would
    // need their own CPS chain — defer until we have a real use case.
    for (i, e) in elems.iter().enumerate() {
        if !pure_expr::is_pure(e) {
            anyhow::bail!(
                "tuple element {i} is not a pure expression (use a `let` to \
                 bind the result of an effectful expression first): {:?}",
                e
            );
        }
    }

    let b = builder.create_block();
    builder.switch_to_block(b);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    // Lazily load vars / self_pid because elements may reference both.
    let needs_vars = elems.iter().any(|e| references_var(e));
    let vars_start = if needs_vars {
        let vars_fp =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::VARIABLES);
        Some(builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0))
    } else {
        None
    };
    let needs_self = elems.iter().any(|e| references_self(e));
    let self_pid = if needs_self {
        Some(builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::OWN_PID,
        ))
    } else {
        None
    };

    // Allocate the payload region (N * 8 bytes).  rt_allocate returns a
    // fat-ptr address — load .start for element stores.
    let alloc_ref = ctx.get_func(builder, "rt_allocate")?;
    let payload_size = builder.ins().iconst(ptr_ty, (elems.len() as i64) * 8);
    let call_alloc = builder.ins().call(alloc_ref, &[payload_size]);
    let fat_ptr = builder.inst_results(call_alloc)[0];
    let payload_start = builder.ins().load(ptr_ty, MemFlags::trusted(), fat_ptr, 0);

    // Fold each element into a Cranelift value and write it into its slot.
    for (i, e) in elems.iter().enumerate() {
        let v = pure_expr::fold_with_self(e, builder, ptr_ty, vars_start, self_pid, state);
        builder
            .ins()
            .store(MemFlags::trusted(), v, payload_start, (i as i32) * 8);
    }

    // Stash the fat-ptr in TEMP_VAL so let_expr / the next CPS block can
    // pick it up.
    let store_ref = ctx.get_func(builder, "rt_store")?;
    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[ctx_ptr, fat_ptr, size, temp_val_offset]);

    let next = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(block_id + 1))
}

fn references_var(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => false,
        Expr::Var(..) => true,
        Expr::Neg(inner) => references_var(&inner.inner),
        Expr::TupleIndex { receiver, .. } => references_var(&receiver.inner),
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
        | Expr::Shr(l, r) => references_var(&l.inner) || references_var(&r.inner),
        _ => false,
    }
}

fn references_self(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => true,
        Expr::Neg(inner) => references_self(&inner.inner),
        Expr::TupleIndex { receiver, .. } => references_self(&receiver.inner),
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
        | Expr::Shr(l, r) => references_self(&l.inner) || references_self(&r.inner),
        _ => false,
    }
}
