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

/// Shape of an arm key — the literal pattern in a `when` arm head.
/// Only the two shapes that combine into an exhaustive 2-arm dispatch
/// are recognised; everything else makes the detector bail out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArmKey {
    Bool(bool),
    Wildcard,
}

/// A statement that may appear *before* the terminating `self(...)`
/// call inside a self-recursive arm body.  Only the two shapes that
/// don't yield the scheduler are accepted; everything else (a
/// nested `when`, a `wait`, a non-pure helper call) makes the
/// detector bail out.
pub enum PreStmt<'src> {
    /// `let X = <pure expr>` — fold the default to a single Cranelift
    /// value, declare a per-iter local binding so subsequent pre-stmts
    /// and the self-args fold can see it by name.
    Let {
        name: &'src str,
        default: Expr<'src>,
    },
    /// Bare scheduler-safe rt-call (e.g. `rt_store(...)`).  After #78
    /// inlines pure stdlib helpers like `set_be32` and `set`, the
    /// expanded body lands here as top-level `rt_store` / `rt_load_*`
    /// / `rt_copy_bytes` invocations.  Args must all be pure.
    SideEffect {
        callee: &'src str,
        args: Vec<Expr<'src>>,
    },
}

/// Information captured from a detected tail-self loop.
///
/// All fields are owned clones so the caller can drive the rest of
/// compilation without holding `&branch.body` borrows.
pub struct TailLoopInfo<'src> {
    pub cond: Expr<'src>,
    /// Statements that run before the terminating `self(...)` call at
    /// each unrolled iteration.  Empty for simple "one-stmt self-arm"
    /// loops like counter / cpu-bench worker; populated for
    /// SHA-256's `fill_w` / `compress` style bodies where the self-
    /// arm has let bindings + bare rt-calls before the recursive
    /// continuation.
    pub pre_stmts: Vec<PreStmt<'src>>,
    pub self_args: Vec<Expr<'src>>,
    pub exit_body: Vec<Expr<'src>>,
    /// `true` if the self-recurring arm's key is `true` (continue iff
    /// cond is non-zero); `false` if the self-arm's key is `false`
    /// (continue iff cond is zero).  Both `counter` (self on `false`)
    /// and the CPU-bench worker (self on `true`) share the same loop
    /// shape; this flag lets the emitter pick the right brif polarity.
    pub continue_when_truthy: bool,
}

/// Whitelist of rt-fns that may appear bare (not let-bound) inside
/// a self-arm body.  These are side-effecting but bounded — they
/// don't yield the scheduler, don't allocate new actors, and can be
/// safely interleaved with the unrolled iterations.
fn is_unroll_safe_side_effect(name: &str) -> bool {
    matches!(
        name,
        "rt_store" | "rt_copy_bytes" | "rt_load_u8" | "rt_load_u16" | "rt_load_u32" | "rt_load_u64"
    )
}

/// Classify a single statement that appears before the final
/// `self(...)` in a self-arm body.  Returns `Some(PreStmt)` when the
/// shape is unroll-safe; otherwise the detector should bail out.
fn classify_pre_stmt<'src>(expr: &Expr<'src>) -> Option<PreStmt<'src>> {
    match expr {
        Expr::Let {
            ident,
            default: Some(d),
            ..
        } => {
            if pure_expr::is_pure(&d.inner) {
                Some(PreStmt::Let {
                    name: ident.inner.0,
                    default: d.inner.clone(),
                })
            } else {
                None
            }
        }
        Expr::Jump { ident, args } => {
            let Expr::Var(name, _) = ident.inner else {
                return None;
            };
            if !is_unroll_safe_side_effect(name) {
                return None;
            }
            if !args.iter().all(|a| pure_expr::is_pure(&a.inner)) {
                return None;
            }
            Some(PreStmt::SideEffect {
                callee: name,
                args: args.iter().map(|a| a.inner.clone()).collect(),
            })
        }
        _ => None,
    }
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

    // After the ret-machine lowering pass, a `-> ret T { when ... }`
    // body is rewritten as `__caller(self, when ...)`, where the
    // `when` becomes the LAST argument of the synthetic `__caller`
    // send.  Peel that wrap so the rest of the detector can pretend
    // the source still has a bare `when` at the top level.  Lake
    // source written without a `ret` annotation (or pre-lowering ASTs)
    // already lands here as a plain `When`.
    let when_expr: &Expr<'src> = match &body[0].inner {
        Expr::When { .. } => &body[0].inner,
        Expr::Jump { ident, args } => {
            let Expr::Var(callee, _) = ident.inner else {
                return None;
            };
            if callee != "__caller" || args.is_empty() {
                return None;
            }
            match &args.last().unwrap().inner {
                w @ Expr::When { .. } => w,
                _ => return None,
            }
        }
        _ => return None,
    };
    let Expr::When { cond, branches } = when_expr else {
        unreachable!()
    };
    if !pure_expr::is_pure(&cond.inner) {
        return None;
    }
    if branches.len() != 2 {
        return None;
    }

    // Identify which arm is the self-tail-call.  The supported arm key
    // shapes are Bool literals (`true` / `false`) or the wildcard `_`.
    // SHA-256's `fill_w` and `compress` use `when i == 64 { true ->
    // exit; _ -> self(...) }` — wildcard for the non-self side — so we
    // need to recognise that as the complementary case to `true`.
    let arm_key_kind = |e: &Expr<'_>| -> Option<ArmKey> {
        match e {
            Expr::Bool(b) => Some(ArmKey::Bool(*b)),
            Expr::Var("_", _) => Some(ArmKey::Wildcard),
            _ => None,
        }
    };

    let mut self_arm_idx: Option<usize> = None;
    let mut self_arm_key: Option<ArmKey> = None;

    // Recognise self-arm candidates: the arm body's LAST stmt must be
    // a `self(pure_args)` Jump, and every earlier stmt must classify
    // as a safe `PreStmt` (let with pure default, or a bare
    // scheduler-safe rt-call).  This is what lets SHA-256's
    // `fill_w` / `compress` bodies — which carry several pre-self
    // let bindings + inlined `rt_store` calls (from #78-inlined
    // `set_be32`) — qualify for unroll alongside the trivial
    // counter / cpu-bench shapes.
    for (i, (key, arm_body)) in branches.iter().enumerate() {
        let Some(k) = arm_key_kind(&key.inner) else {
            return None;
        };

        if let Some(last) = arm_body.last() {
            if let Expr::Jump { ident, args } = &last.inner {
                if let Expr::Var(name, _) = ident.inner {
                    let is_self_call = (name == "self" || name == machine_ident)
                        && args.len() == expected_arg_count
                        && args.iter().all(|a| pure_expr::is_pure(&a.inner));
                    let pre_ok = arm_body[..arm_body.len() - 1]
                        .iter()
                        .all(|s| classify_pre_stmt(&s.inner).is_some());
                    if is_self_call && pre_ok && self_arm_idx.is_none() {
                        self_arm_idx = Some(i);
                        self_arm_key = Some(k);
                        continue;
                    }
                }
            }
        }
        // Non-self arm — accept any body; will be compiled as exit-arm.
    }

    let self_idx = self_arm_idx?;
    let self_key = self_arm_key?;
    let exit_idx = 1 - self_idx;
    let exit_key = arm_key_kind(&branches[exit_idx].0.inner)?;

    // The (self_key, exit_key) pair must cover the discriminant
    // exhaustively.  Accepted shapes:
    //   (Bool(true),  Bool(false))   ← counter / cpu-bench worker
    //   (Bool(false), Bool(true))    ← sum
    //   (Bool(b),     Wildcard)      ← sha256 fill_w / compress
    //   (Wildcard,    Bool(b))       ← (theoretical mirror — covered)
    let (self_key_truthy, exit_key_ok) = match (self_key, exit_key) {
        (ArmKey::Bool(b), ArmKey::Bool(c)) if b != c => (b, true),
        (ArmKey::Bool(b), ArmKey::Wildcard) => (b, true),
        (ArmKey::Wildcard, ArmKey::Bool(b)) => (!b, true),
        _ => (false, false),
    };
    if !exit_key_ok {
        return None;
    }

    let self_arm_body = &branches[self_idx].1;
    let exit_arm_body = &branches[exit_idx].1;

    let last_idx = self_arm_body.len() - 1;
    let Expr::Jump { args, .. } = &self_arm_body[last_idx].inner else {
        unreachable!("self-arm last-stmt shape was already verified")
    };
    let self_args: Vec<Expr<'src>> = args.iter().map(|a| a.inner.clone()).collect();

    // Reclassify pre-stmts now that we know they're all valid; the
    // detect loop above ran the `is_some()` check, so unwraps here
    // can never fail.
    let pre_stmts: Vec<PreStmt<'src>> = self_arm_body[..last_idx]
        .iter()
        .map(|s| classify_pre_stmt(&s.inner).expect("pre-stmt was already classified"))
        .collect();

    let cond_expr = cond.inner.clone();
    let exit_body: Vec<Expr<'src>> = exit_arm_body.iter().map(|e| e.inner.clone()).collect();

    Some(TailLoopInfo {
        cond: cond_expr,
        pre_stmts,
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
        let cond_val = pure_expr::fold(&info.cond, builder, ptr_ty, Some(vars_start), state);
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

        // ── pre-stmts: let-bindings + bare scheduler-safe rt-calls
        //    that appear before the final `self(...)` in the source
        //    arm body.  Each Let introduces a per-iter binding so
        //    subsequent reads (and the self-args fold) see it by
        //    name through `state.cached_vars`.  After this iter
        //    finishes we drop those bindings so the next iter's
        //    redeclaration of the same name gets a fresh Cranelift
        //    Variable.
        let mut iter_locals: Vec<&str> = Vec::new();
        for pre in &info.pre_stmts {
            match pre {
                PreStmt::Let { name, default } => {
                    let val = pure_expr::fold(default, builder, ptr_ty, Some(vars_start), state);
                    let v = builder.declare_var(ptr_ty);
                    builder.def_var(v, val);
                    let slot = state.insert(name.to_string(), ptr_ty);
                    state.cache_slot(slot, v);
                    iter_locals.push(name);
                }
                PreStmt::SideEffect { callee, args } => {
                    let func_ref = ctx.get_func(builder, callee)?;
                    let arg_vals: Vec<_> = args
                        .iter()
                        .map(|a| pure_expr::fold(a, builder, ptr_ty, Some(vars_start), state))
                        .collect();
                    builder.ins().call(func_ref, &arg_vals);
                }
            }
        }

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

        // Drop per-iter locals before the next chunk redefines them.
        for name in iter_locals {
            state.remove(name);
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
    let new_remaining = builder.ins().iadd_imm(remaining, -(unroll_factor as i64));
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
