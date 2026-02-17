use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Type, Value, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome},
    rt::layout::ExecCtxLayout,
};

pub fn is_pure(expr: &Expr) -> bool {
    match expr {
        Expr::Num(..) | Expr::Bool(..) | Expr::Var(..) => true,
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => is_pure(&l.inner) && is_pure(&r.inner),
        _ => false,
    }
}

fn has_var(expr: &Expr) -> bool {
    match expr {
        Expr::Var(..) => true,
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => has_var(&l.inner) || has_var(&r.inner),
        _ => false,
    }
}

fn fold(
    expr: &Expr,
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    vars_start: Option<Value>,
    state: &BranchState,
) -> Value {
    match expr {
        Expr::Num(s, _) => builder.ins().iconst(ptr_ty, s.parse::<i64>().unwrap_or(0)),
        Expr::Bool(b) => builder.ins().iconst(ptr_ty, if *b { 1 } else { 0 }),
        Expr::Var(name, _) => {
            let (_, slot) = state.get(name).expect("variable not found in state");
            debug_assert!(slot < state.len(), "slot {slot} out of range {}", state.len());
            let vs = vars_start.expect("vars_start missing for Var node");
            builder.ins().load(ptr_ty, MemFlags::trusted(), vs, slot as i32 * 8)
        }
        Expr::Add(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            builder.ins().iadd(lv, rv)
        }
        Expr::Sub(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            builder.ins().isub(lv, rv)
        }
        Expr::Mul(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            builder.ins().imul(lv, rv)
        }
        Expr::Div(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            builder.ins().sdiv(lv, rv)
        }
        Expr::Le(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            let cmp = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Ge(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            let cmp = builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Eq(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            let cmp = builder.ins().icmp(IntCC::Equal, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Lt(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            let cmp = builder.ins().icmp(IntCC::SignedLessThan, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Gt(l, r) => {
            let lv = fold(&l.inner, builder, ptr_ty, vars_start, state);
            let rv = fold(&r.inner, builder, ptr_ty, vars_start, state);
            let cmp = builder.ins().icmp(IntCC::SignedGreaterThan, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        _ => unreachable!("fold called on non-pure expr: {:?}", expr),
    }
}

pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    expr: &Expr,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let b = builder.create_block();
    builder.switch_to_block(b);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    let vars_start = if has_var(expr) {
        let vars_fp = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::VARIABLES);
        let start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0);
        Some(start)
    } else {
        None
    };

    let result = fold(expr, builder, ptr_ty, vars_start, state);

    builder
        .ins()
        .store(MemFlags::trusted(), result, exec_start, ExecCtxLayout::TEMP_VAL);

    let next = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next)]);

    branch_switch.set_entry(block_id as u128, b);

    Ok(StmtOutcome::Continue(block_id + 1))
}
