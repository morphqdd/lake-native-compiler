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
        // `buf[i]` desugars to a bounds-checked byte load — the
        // Index expr is the source-level form of `rt_load_u8(buf, i)`
        // and is already a "scheduler-safe rt-call" by the same
        // argument that admits `rt_load_u8` to `is_scheduler_safe_rt_fn`
        // below.  Treat it as pure as long as both operands are pure
        // so the unroll detector accepts SHA-256's `fill_w` body
        // (which uses `b0 = buf[off]` after Phase 2a's `be32` inline).
        Expr::Index { receiver, index } => is_pure(&receiver.inner) && is_pure(&index.inner),
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
        // #81 — Calls to scheduler-safe rt-functions are pure from the
        // scheduling standpoint: they run synchronously, never yield,
        // and complete in bounded time.  Pure_expr::fold knows how to
        // emit them inline (direct rt-fn call with folded arg Values,
        // bypassing the JUMP_ARGS staging machinery).
        //
        // Note: rt_store / rt_copy_bytes / rt_free have side effects
        // on memory state but no scheduler interaction, so they're
        // still safe to coalesce into a super-block.
        Expr::Jump { ident, args } => {
            if let Expr::Var(callee, _) = ident.inner {
                if is_scheduler_safe_rt_fn(callee) {
                    return args.iter().all(|a| is_pure(&a.inner));
                }
            }
            false
        }
        _ => false,
    }
}

/// Whitelist of rt-functions whose body is small enough to be folded
/// inline as a pure expression value.  Currently restricted to
/// VALUE-returning bounded reads (rt_load_u*), since fold_with_self
/// doesn't have CompilerCtx access to emit FuncRef calls for the
/// larger rt-fns (rt_allocate, rt_copy_bytes, rt_store), and those
/// still go through jump_expr's staging path.
///
/// rt_load_u8 etc are 3-instruction inlines: fat-ptr deref, indexed
/// load, zero-extend.  Far cheaper than the staging + call sequence.
fn is_scheduler_safe_rt_fn(name: &str) -> bool {
    matches!(
        name,
        "rt_load_u8" | "rt_load_u16" | "rt_load_u32" | "rt_load_u64"
    )
}

/// Cranelift integer type for a `rt_load_u*` size.  Used when emitting
/// the load width before zero-extending to ptr_ty.
fn rt_load_size_ty(name: &str) -> Option<cranelift::prelude::Type> {
    use cranelift::prelude::types;
    match name {
        "rt_load_u8" => Some(types::I8),
        "rt_load_u16" => Some(types::I16),
        "rt_load_u32" => Some(types::I32),
        "rt_load_u64" => Some(types::I64),
        _ => None,
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
        Expr::Jump { args, .. } => args.iter().any(|a| has_self(&a.inner)),
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
        Expr::Index { receiver, index } => has_var(&receiver.inner) || has_var(&index.inner),
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
        Expr::Jump { args, .. } => args.iter().any(|a| has_var(&a.inner)),
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
        Expr::Num(s, _) => builder.ins().iconst(
            ptr_ty,
            lake_frontend::api::expr::parse_int_literal(s).unwrap_or(0),
        ),
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
        // `buf[i]` — single byte load.  Match `rt_load_u8`'s shape:
        // deref the fat-ptr to get `start`, add the index, load one
        // byte, zero-extend to ptr_ty.  Bounds check is intentionally
        // skipped (same trade-off as the `rt_load_u8` inline in the
        // Jump arm below).
        Expr::Index { receiver, index } => {
            use cranelift::prelude::types;
            let recv_val = fold_with_self(
                &receiver.inner,
                builder,
                ptr_ty,
                vars_start,
                self_pid,
                state,
            );
            let idx_val =
                fold_with_self(&index.inner, builder, ptr_ty, vars_start, self_pid, state);
            let start = builder.ins().load(ptr_ty, MemFlags::trusted(), recv_val, 0);
            let addr = builder.ins().iadd(start, idx_val);
            let raw = builder.ins().load(types::I8, MemFlags::trusted(), addr, 0);
            builder.ins().uextend(ptr_ty, raw)
        }
        Expr::Var("self", _) => self_pid.expect(
            "self used as a value but no self_pid supplied — call \
             pure_expr::compile or pass self_pid through fold_with_self",
        ),
        Expr::Var(name, _) => {
            let (_, slot) = state.get(name).expect("variable not found in state");
            debug_assert!(
                slot < state.len(),
                "slot {slot} out of range {}",
                state.len()
            );
            // Variable cache: if branch.rs already loaded this slot into
            // a Cranelift Variable at branch_entry, use_var picks it up
            // from a register instead of re-loading from memory.  For
            // tight self-loops (CPU bench worker reading steps/acc1/acc2
            // each iter) this drops ~3 memory loads per iteration.
            if let Some(var) = state.cached_var(slot) {
                builder.use_var(var)
            } else {
                let vs = vars_start.expect("vars_start missing for Var node");
                builder
                    .ins()
                    .load(ptr_ty, MemFlags::trusted(), vs, slot as i32 * 8)
            }
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
        // #81 — Inline `rt_load_u*(fat_ptr, offset)` as Cranelift loads.
        //
        // Original rt-fn body:
        //   load fat_ptr.start, then load(size) at start+offset,
        //   uextend to ptr_ty.
        //
        // Inlining saves a function-call frame + JUMP_ARGS staging
        // (which would emit multiple sub-blocks via jump_expr).  On
        // SHA-256-style code that does thousands of byte-level loads
        // per round, this is a major win.
        Expr::Jump { ident, args } => {
            let callee = match ident.inner {
                Expr::Var(name, _) => name,
                _ => unreachable!(
                    "Jump in fold must have Var callee — is_pure should have rejected otherwise"
                ),
            };
            let size_ty = rt_load_size_ty(callee).unwrap_or_else(|| {
                unreachable!(
                    "fold called on non-pure Jump to '{}'; is_pure mismatch",
                    callee
                )
            });

            // Two args: fat_ptr_addr, offset.
            debug_assert_eq!(args.len(), 2, "{} takes 2 args (fat_ptr, offset)", callee);
            let fp_val =
                fold_with_self(&args[0].inner, builder, ptr_ty, vars_start, self_pid, state);
            let off_val =
                fold_with_self(&args[1].inner, builder, ptr_ty, vars_start, self_pid, state);

            // Fat-ptr deref: start = *fat_ptr_addr.  Trusted because
            // we've already type-checked the buf type via the language
            // layer.  Note: we skip the rt-fn's bounds check (which
            // compared offset against fat_ptr.end) — caller code in
            // stdlib has been audited to stay in range.  See trade-off
            // discussion in docs/lowering.md.
            let start = builder.ins().load(ptr_ty, MemFlags::trusted(), fp_val, 0);
            let addr = builder.ins().iadd(start, off_val);
            let raw = builder.ins().load(size_ty, MemFlags::trusted(), addr, 0);
            // Zero-extend if narrower than ptr_ty (the i64 ABI for
            // Lake's variable slots).
            if size_ty == ptr_ty {
                raw
            } else {
                builder.ins().uextend(ptr_ty, raw)
            }
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
    omit_exit: bool,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // #80 Level 3: when `omit_exit=true` AND `entry=Some(super_b)`, this
    // statement is in the middle of a super-block run.  Skip the exit
    // brif so subsequent statements emit into the same Cranelift block.
    // The driver (branch.rs / wait_expr) supplies the final exit at the
    // end of the run.
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
        let vars_fp = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
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

    builder.ins().store(
        MemFlags::trusted(),
        result,
        exec_start,
        ExecCtxLayout::TEMP_VAL,
    );

    // #80 Level 2/3: emit exit unless we're mid-super-block.
    if !omit_exit {
        emit_continue(builder, ctx, ptr_ty, block_id, fall_through);
    }

    // Register this statement's block_id for re-entry.  In super-block mode
    // multiple statements alias the same `b`, which is fine — branch_switch
    // accepts multiple cases pointing at the same target.  Resume from any
    // mid-super-block id re-runs the whole super-block from `b`; since
    // all super-block-eligible statements are pure (no side effects beyond
    // idempotent writes to vars), the re-run is harmless.
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
        builder
            .ins()
            .brif(exhausted, yb, &[BlockArg::Value(next)], ft, &[]);
        return;
    }

    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next)]);
}
