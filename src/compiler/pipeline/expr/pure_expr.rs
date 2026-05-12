use anyhow::Result;
use cranelift::{
    codegen::ir::{Block, BlockArg},
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
        Expr::Num(..) | Expr::Bool(..) | Expr::Var(..) | Expr::Atom(..) => true,
        Expr::Neg(inner) => is_pure(&inner.inner),
        Expr::TupleIndex { receiver, .. } => is_pure(&receiver.inner),
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
        | Expr::Shr(l, r) => is_pure(&l.inner) && is_pure(&r.inner),
        _ => false,
    }
}

/// Stable compile-time mapping from an atom name to its runtime ID.
///
/// Two `:ok` literals anywhere in the program fold to the same `i64`,
/// which is what equality and pattern dispatch rely on.  We use the
/// stable `DefaultHasher` (SipHash) and force the low bit so the value
/// is always non-zero — `0` stays available as the "no atom" sentinel.
pub fn atom_id(name: &str) -> i64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    (h.finish() as i64) | 1
}

/// Does this expression mention `self`?  Used by [`compile`] to decide
/// whether to plumb the current process's pid through to [`fold_with_self`].
fn has_self(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => true,
        Expr::Neg(inner) => has_self(&inner.inner),
        Expr::TupleIndex { receiver, .. } => has_self(&receiver.inner),
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
        | Expr::Shr(l, r) => has_self(&l.inner) || has_self(&r.inner),
        _ => false,
    }
}

fn has_var(expr: &Expr) -> bool {
    match expr {
        // `self` resolves to the current pid via `machine_ctx_var`, not
        // through the variables table, so it doesn't count as a "var" for
        // the purposes of deciding whether to load the vars buffer.
        Expr::Var("self", _) => false,
        Expr::Var(..) => true,
        Expr::Neg(inner) => has_var(&inner.inner),
        Expr::TupleIndex { receiver, .. } => has_var(&receiver.inner),
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
        | Expr::Shr(l, r) => has_var(&l.inner) || has_var(&r.inner),
        _ => false,
    }
}

pub fn fold(
    expr: &Expr,
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    vars_start: Option<Value>,
    state: &BranchState,
) -> Value {
    fold_with_self(expr, builder, ptr_ty, vars_start, None, state)
}

/// Like [`fold`] but with an extra `self_pid` parameter — the value to
/// substitute for `Var("self")`.  Callers that want to use `self` as a
/// pure value (e.g. as an argument to another machine) provide the
/// current process's pid here; existing call sites that don't yet
/// support `self` use [`fold`].
pub fn fold_with_self(
    expr: &Expr,
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    vars_start: Option<Value>,
    self_pid: Option<Value>,
    state: &BranchState,
) -> Value {
    match expr {
        Expr::Num(s, _) => builder
            .ins()
            .iconst(ptr_ty, lake_frontend::api::expr::parse_int_literal(s).unwrap_or(0)),
        Expr::Bool(b) => builder.ins().iconst(ptr_ty, if *b { 1 } else { 0 }),
        Expr::Atom(name) => builder.ins().iconst(ptr_ty, atom_id(name)),
        Expr::TupleIndex { receiver, index } => {
            let recv_val = fold_with_self(
                &receiver.inner,
                builder,
                ptr_ty,
                vars_start,
                self_pid,
                state,
            );
            // The receiver is the address of a 16 B fat-ptr `{start, end}`.
            // Tuple payload is `[elem0, elem1, ...]`, each i64-sized.
            let start = builder.ins().load(ptr_ty, MemFlags::trusted(), recv_val, 0);
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), start, (*index as i32) * 8)
        }
        Expr::Var("self", _) => self_pid.expect(
            "self used as a value but no self_pid supplied — call \
             pure_expr::compile or pass self_pid through fold_with_self",
        ),
        Expr::Var(name, _) => {
            let (_, slot) = state.get(name).expect("variable not found in state");
            debug_assert!(slot < state.len(), "slot {slot} out of range {}", state.len());
            let vs = vars_start.expect("vars_start missing for Var node");
            builder.ins().load(ptr_ty, MemFlags::trusted(), vs, slot as i32 * 8)
        }
        Expr::Add(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().iadd(lv, rv)
        }
        Expr::Sub(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().isub(lv, rv)
        }
        Expr::Mul(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().imul(lv, rv)
        }
        Expr::Div(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().sdiv(lv, rv)
        }
        Expr::Le(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            let cmp = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Ge(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            let cmp = builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Eq(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            let cmp = builder.ins().icmp(IntCC::Equal, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Lt(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            let cmp = builder.ins().icmp(IntCC::SignedLessThan, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Gt(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            let cmp = builder.ins().icmp(IntCC::SignedGreaterThan, lv, rv);
            builder.ins().uextend(ptr_ty, cmp)
        }
        Expr::Neg(inner) => {
            let v = fold_with_self(&inner.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().ineg(v)
        }
        Expr::BAnd(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().band(lv, rv)
        }
        Expr::BOr(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().bor(lv, rv)
        }
        Expr::BXor(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().bxor(lv, rv)
        }
        Expr::Shl(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            builder.ins().ishl(lv, rv)
        }
        Expr::Shr(l, r) => {
            let lv = fold_with_self(&l.inner, builder, ptr_ty, vars_start, self_pid, state);
            let rv = fold_with_self(&r.inner, builder, ptr_ty, vars_start, self_pid, state);
            // Logical / unsigned right shift — what crypto code expects.
            builder.ins().ushr(lv, rv)
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
    entry: Option<Block>,
    fall_through: Option<Block>,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // #80 Level 2: use the caller-provided entry block if available
    // (chaining from a previous fast-path fall_through), else create our own.
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

    // `self` as a value resolves to the current actor's pid (= the
    // process_ctx fat-ptr address that other actors use to send to it).
    // It's stashed in this actor's ExecCtx at OWN_PID by spawn /
    // init_main_process; load it on demand.
    let self_pid = if has_self(expr) {
        Some(builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::OWN_PID,
        ))
    } else {
        None
    };

    let result = fold_with_self(expr, builder, ptr_ty, vars_start, self_pid, state);

    builder
        .ins()
        .store(MemFlags::trusted(), result, exec_start, ExecCtxLayout::TEMP_VAL);

    // #80 Level 2: fast-path exit emits inline quantum check + brif.
    emit_continue(builder, ctx, ptr_ty, block_id, fall_through);

    branch_switch.set_entry(block_id as u128, b);

    Ok(StmtOutcome::Continue(block_id + 1))
}

/// #80 Level 2 — shared exit emitter.  When `fall_through` is `Some` and
/// the machine has registered both `quantum_var` and `yield_block`, emit
/// `dec quantum; brif zero, fast_yield[next], fall_through`; otherwise
/// fall back to the legacy `jump quantum_block(next)` path.
pub fn emit_continue(
    builder: &mut FunctionBuilder,
    ctx: &CompilerCtx,
    ptr_ty: Type,
    block_id: i64,
    fall_through: Option<Block>,
) {
    let next = builder.ins().iconst(ptr_ty, block_id + 1);

    if let Some(ft) = fall_through
        && let (Some(qv), Some(yb)) = (ctx.quantum_var(), ctx.yield_block())
    {
        let remaining = builder.use_var(qv);
        let new_remaining = builder.ins().iadd_imm(remaining, -1);
        builder.def_var(qv, new_remaining);
        let exhausted = builder.ins().icmp_imm(IntCC::Equal, new_remaining, 0);
        builder.ins().brif(
            exhausted,
            yb,
            &[BlockArg::Value(next)],
            ft,
            &[],
        );
        return;
    }

    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next)]);
}
