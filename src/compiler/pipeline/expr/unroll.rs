//! #85 — Loop unroll for tail-self ret-machines.
//!
//! Detects the canonical actor-loop shape:
//!
//! ```text
//! branch.body == [
//!   When { pure_cond,
//!     branches: [
//!       (Bool(true),  [Jump self pure_args]),
//!       (Bool(false), [arbitrary exit body])
//!     ]
//!   }
//! ]
//! ```
//!
//! and emits an unrolled version of the loop body so multiple logical
//! iterations execute inside a single Cranelift basic block.  Quantum
//! is decremented by `U` once per unrolled chunk (vs once per iter),
//! and the dispatch chain `branch_entry → branch_switch → body[0]` is
//! avoided between iters because we use a direct Cranelift back-edge.
//!
//! ## Concurrency guarantees
//!
//! Preemption granularity becomes `U` iters (vs 1).  Worst-case
//! quantum overshoot is `0` because we pre-check `quantum - U >= 0`
//! BEFORE running the next chunk; the actor yields slightly earlier
//! instead of slightly later.  Fairness is preserved up to a constant
//! factor `U`.
//!
//! Mid-chunk exit (cond flips false on iter `K < U`) commits the
//! current register-resident var state back to memory and jumps to
//! the exit-arm body via the normal `qb` path.  Resume after a
//! `STOP_LIMIT` yield rehydrates vars from memory at `entry_load`.

use std::collections::HashMap;

use anyhow::Result;
use chumsky::span::Spanned;
use cranelift::{
    codegen::ir::{Block, BlockArg},
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, pure_expr},
    rt::layout::ExecCtxLayout,
};

/// Information captured from a detected tail-self loop.
///
/// All fields are owned clones so the caller can drive the rest of
/// compilation without holding `&branch.body` borrows.
pub struct TailLoopInfo<'src> {
    pub cond: Expr<'src>,
    pub self_args: Vec<Expr<'src>>,
    pub exit_body: Vec<Expr<'src>>,
    /// `true` if the self-recurring arm's key is `true` (continue iff
    /// cond is non-zero); `false` if the self-arm's key is `false`
    /// (continue iff cond is zero).  Both `counter` (self on `false`)
    /// and the CPU-bench worker (self on `true`) share the same loop
    /// shape; this flag lets the emitter pick the right brif polarity.
    pub continue_when_truthy: bool,
}

/// Returns `Some(info)` when `body` matches the supported tail-self loop
/// shape AND every `self()` arg is a pure expression that touches only
/// the named pattern slots.
///
/// Rejects (returns `None`) when:
/// * body has more than one top-level stmt,
/// * top-level stmt isn't a `when`,
/// * the `when` has != 2 arms or arm keys aren't `true`/`false` bools,
/// * neither arm body is exactly a single `self(...)` jump,
/// * any `self` arg / the cond is impure (side-effects forbidden inside
///   the unrolled chunk),
/// * `arg_count` exceeds the named slots (extra positional writes that
///   the unrolled path can't track in Cranelift Variables).
pub fn detect_tail_loop<'src>(
    body: &[Spanned<Expr<'src>>],
    machine_ident: &str,
    expected_arg_count: usize,
) -> Option<TailLoopInfo<'src>> {
    if body.len() != 1 {
        return None;
    }
    let Expr::When { cond, branches } = &body[0].inner else {
        return None;
    };
    if !pure_expr::is_pure(&cond.inner) {
        return None;
    }
    if branches.len() != 2 {
        return None;
    }

    // Identify which arm is the self-tail-call.  Accept either arm key as
    // long as it's a Bool literal — `counter` uses `false -> self`,
    // CPU-bench's worker uses `true -> self`.  The other arm key must
    // be the complementary Bool so the dispatch is total.
    let mut self_arm_idx: Option<usize> = None;
    let mut self_arm_key_truthy: Option<bool> = None;

    for (i, (key, arm_body)) in branches.iter().enumerate() {
        let key_truthy = match key.inner {
            Expr::Bool(b) => b,
            _ => return None,
        };

        if arm_body.len() == 1 {
            if let Expr::Jump { ident, args } = &arm_body[0].inner {
                if let Expr::Var(name, _) = ident.inner {
                    if (name == "self" || name == machine_ident)
                        && args.len() == expected_arg_count
                        && args.iter().all(|a| pure_expr::is_pure(&a.inner))
                        && self_arm_idx.is_none()
                    {
                        self_arm_idx = Some(i);
                        self_arm_key_truthy = Some(key_truthy);
                        continue;
                    }
                }
            }
        }
        // Non-self arm — accept any body; will be compiled as exit-arm.
    }

    let self_idx = self_arm_idx?;
    let self_key_truthy = self_arm_key_truthy?;
    let exit_idx = 1 - self_idx;

    // The exit-arm key must be the complementary Bool so the two arms
    // cover the discriminant exhaustively.
    let exit_key_ok = matches!(
        branches[exit_idx].0.inner,
        Expr::Bool(b) if b != self_key_truthy
    );
    if !exit_key_ok {
        return None;
    }

    let self_arm_body = &branches[self_idx].1;
    let exit_arm_body = &branches[exit_idx].1;

    let Expr::Jump { args, .. } = &self_arm_body[0].inner else {
        unreachable!("self_arm shape was already verified")
    };
    let self_args: Vec<Expr<'src>> = args.iter().map(|a| a.inner.clone()).collect();
    let cond_expr = cond.inner.clone();
    let exit_body: Vec<Expr<'src>> = exit_arm_body.iter().map(|e| e.inner.clone()).collect();

    Some(TailLoopInfo {
        cond: cond_expr,
        self_args,
        exit_body,
        continue_when_truthy: self_key_truthy,
    })
}

/// Emit the unrolled loop body into the current function.
///
/// Pre-conditions (caller's contract):
/// * `branch_entry_block` was created by `branch.rs` and is the
///   destination of `machine_switch[branch_id]`.  It is **empty** —
///   we fill it here.
/// * `branch_switch_block` was created by `branch.rs` and takes the
///   loaded `BLOCK_ID` as its single block param.  We register
///   `BLOCK_ID = 0 → entry_load` in the returned `Switch` and the
///   caller emits the switch at the end of branch compilation.
/// * `state` contains slot entries for every named pattern position
///   (positional, i.e. `named_slots[k] == k`).
/// * `ctx.quantum_var()` and `ctx.yield_block()` are set (true after
///   `compile_machine` prepares the inner loop).
///
/// Returns `(branch_switch, next_block_id)` so the caller can emit
/// the exit-arm body starting at `next_block_id` using the normal
/// `compile_expr` machinery.  The returned `Switch` already has
/// `set_entry(0, entry_load)` populated; the caller is responsible
/// for `switch.emit(...)` at branch_switch_block.
pub fn compile_unrolled_branch<'src>(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    branch_entry_block: Block,
    branch_switch_block: Block,
    info: &TailLoopInfo<'src>,
    unroll_factor: usize,
    state: &mut BranchState,
    named_slots: &[usize],
) -> Result<(Switch, i64)> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let unroll_factor = unroll_factor.max(2);

    let mut branch_switch = Switch::new();

    let qv = ctx
        .quantum_var()
        .expect("quantum_var must be set before compile_unrolled_branch");
    let yield_block = ctx
        .yield_block()
        .expect("yield_block must be set before compile_unrolled_branch");

    // ── branch_entry: load BLOCK_ID and jump to switch ─────────────────
    builder.switch_to_block(branch_entry_block);
    let exec_start_be = ctx.exec_start(builder, machine_ctx_var);
    let block_id_load = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start_be,
        ExecCtxLayout::BLOCK_ID,
    );
    builder
        .ins()
        .jump(branch_switch_block, &[BlockArg::Value(block_id_load)]);

    // ── entry_load: rehydrate Cranelift Variables from memory on every
    //    re-entry (initial dispatch, post-yield resume).
    let entry_load = builder.create_block();
    branch_switch.set_entry(0, entry_load);
    builder.switch_to_block(entry_load);

    // Declare a Cranelift Variable for each named slot.
    let mut slot_vars: HashMap<usize, Variable> = HashMap::new();
    for &slot in named_slots {
        let v = builder.declare_var(ptr_ty);
        slot_vars.insert(slot, v);
    }

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let vars_fp = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::VARIABLES,
    );
    let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0);

    for &slot in named_slots {
        let v = slot_vars[&slot];
        let loaded = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), vars_start, slot as i32 * 8);
        builder.def_var(v, loaded);
    }

    // Make register-resident vars visible to pure_expr::fold.
    for (&slot, &v) in slot_vars.iter() {
        state.cache_slot(slot, v);
    }

    // ── loop_top: back-edge target.  Holds the unrolled chunk; iter 0..U
    //    each evaluates the cond, branches to exit_bridge on false, else
    //    folds new arg values and def_var's them.
    let loop_top = builder.create_block();
    let exit_bridge = builder.create_block();

    builder.ins().jump(loop_top, &[]);
    builder.switch_to_block(loop_top);

    for _iter in 0..unroll_factor {
        // Cond folds against the current Cranelift Variable values.
        let cond_val = pure_expr::fold(
            &info.cond,
            builder,
            ptr_ty,
            Some(vars_start),
            state,
        );
        // If self-arm key was `true`, continue iff cond is non-zero.
        // If self-arm key was `false`, continue iff cond is zero (i.e.
        // when 0 == n { true → exit; false → self } loops while n != 0).
        let cmp_cc = if info.continue_when_truthy {
            IntCC::NotEqual
        } else {
            IntCC::Equal
        };
        let take_iter = builder.ins().icmp_imm(cmp_cc, cond_val, 0);
        let body_block = builder.create_block();
        builder
            .ins()
            .brif(take_iter, body_block, &[], exit_bridge, &[]);
        builder.switch_to_block(body_block);

        // Fold all self() args first (don't write any var slot until ALL
        // reads are done — `self(steps-1 acc2 acc1+acc2)` must read the
        // original acc2 before overwriting it).
        let new_vals: Vec<_> = info
            .self_args
            .iter()
            .map(|a| pure_expr::fold(a, builder, ptr_ty, Some(vars_start), state))
            .collect();

        for (i, val) in new_vals.iter().enumerate() {
            if let Some(&v) = slot_vars.get(&i) {
                builder.def_var(v, *val);
            }
        }
    }

    // ── after U successful iters: commit Cranelift Vars → memory, dec
    //    quantum by U, brif yield or back-edge to loop_top.
    for &slot in named_slots {
        let v = slot_vars[&slot];
        let cur = builder.use_var(v);
        builder
            .ins()
            .store(MemFlags::trusted(), cur, vars_start, slot as i32 * 8);
    }

    let remaining = builder.use_var(qv);
    let new_remaining = builder
        .ins()
        .iadd_imm(remaining, -(unroll_factor as i64));
    builder.def_var(qv, new_remaining);

    let exhausted = builder
        .ins()
        .icmp_imm(IntCC::SignedLessThanOrEqual, new_remaining, 0);
    let zero = builder.ins().iconst(ptr_ty, 0);
    builder.ins().brif(
        exhausted,
        yield_block,
        &[BlockArg::Value(zero)],
        loop_top,
        &[],
    );

    // ── exit_bridge: cond was false at some iter K (0 ≤ K < U).  Commit
    //    the current var state and route to the exit-arm body through qb.
    builder.switch_to_block(exit_bridge);
    let ctx_ptr2 = builder.use_var(machine_ctx_var);
    let exec_start2 = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr2, 0);
    let vars_fp2 = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start2,
        ExecCtxLayout::VARIABLES,
    );
    let vars_start2 = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp2, 0);
    for &slot in named_slots {
        let v = slot_vars[&slot];
        let cur = builder.use_var(v);
        builder
            .ins()
            .store(MemFlags::trusted(), cur, vars_start2, slot as i32 * 8);
    }

    // Drop the var-cache: exit body uses the normal memory-load path.
    state.clear_var_cache();

    let one = builder.ins().iconst(ptr_ty, 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(one)]);

    Ok((branch_switch, 1))
}
