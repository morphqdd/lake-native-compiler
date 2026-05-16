use anyhow::{Result, anyhow, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    hash_call_args,
    pipeline::{
        expr::{
            BranchState, StmtOutcome, change_state_expr, compile_expr, dispatch, pure_expr,
            send_expr, spawn_expr,
        },
        machine::STOP_PARK,
    },
    rt::layout::ExecCtxLayout,
};

/// Compile a jump / function call: `callee(arg0, arg1, ...)`.
///
/// For each argument:
///   1. Compile the argument expression (leaves result in TEMP_VAL).
///   2. Open a new block that reads TEMP_VAL and writes it into JUMP_ARGS[i].
///
/// Then open a final block that loads all args from JUMP_ARGS and calls the
/// target machine, returning -1 to signal the scheduler that this branch is done.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    ident: &Expr<'_>,
    args: &[Expr<'_>],
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let Expr::Var(callee_name, _ty) = ident else {
        bail!("Jump target must be a variable/identifier");
    };

    // ── Fused self-call: if callee is "self" and all args are pure,
    //    emit a single block that computes args inline and writes directly
    //    to VARIABLES, bypassing TEMP_VAL / JUMP_ARGS staging entirely.
    if *callee_name == "self" && args.iter().all(|a| pure_expr::is_pure(a)) {
        if let Some(machine_name) = ctx.get_current_machine() {
            let call_hash = hash_call_args(args, state.lake_types());
            return compile_fused_self_call(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                &machine_name,
                call_hash,
                args,
            );
        }
    }

    // ── Fused rt-call: scheduler-safe rt-fn with all-pure args can
    //    skip JUMP_ARGS staging entirely.  Fold each arg as a Cranelift
    //    Value, emit direct call to the rt fn with those values, store
    //    the return into TEMP_VAL (if any), single CPS block.
    //
    //    Big win for stdlib helpers that do many rt-calls per branch
    //    (set_be32 = 4 rt_store calls; init_k_table = 64 set_be32 calls;
    //    SHA-256 hot path uses these heavily).
    // Park-aware rt fns need the slow path's BLOCK_ID + STOP_PARK
    // epilogue.  Calling them through the fused fast path bails with
    // "park-aware rt-fn with all-pure args — not yet supported on
    // fused path" — keep them off the fast path entirely so TCP /
    // io_uring stdlib wrappers (where all args are typically Var or
    // Num and would otherwise qualify) just work.
    let is_park_aware = matches!(
        *callee_name,
        "rt_io_park_current" | "rt_accept_async" | "rt_send_async" | "rt_recv_async"
    );
    if !is_park_aware
        && ctx.is_declared_rt_func_in_prog(callee_name)
        && args.iter().all(|a| pure_expr::is_pure(a))
    {
        return compile_fused_rt_call(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            callee_name,
            args,
        );
    }

    let call_base = state.jump_args_base;

    let mut next_id = block_id;

    for (i, arg) in args.iter().enumerate() {
        state.jump_args_base = call_base + args.len();

        // Argument expressions must produce a value (Continue).
        // A terminal (StateChange, Wait, …) has no return value to pass.
        let after_arg_id = match compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            next_id,
            branch_switch,
            state,
            arg,
            None,
            None,
            false,
        )? {
            StmtOutcome::Continue(id) => id,
            other => bail!(
                "argument #{} to '{}' is a terminal expression ({:?}); \
                 terminals have no return value and cannot be used as arguments",
                i,
                callee_name,
                other
            ),
        };

        state.jump_args_base = call_base;

        let b = builder.create_block();
        builder.switch_to_block(b);

        // #81 — inline rt_load_u64 / rt_store (scheduler-trusted).
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        let arg_val = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::TEMP_VAL,
        );
        let ja_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::JUMP_ARGS,
        );
        let ja_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ja_fat, 0);
        builder.ins().store(
            MemFlags::trusted(),
            arg_val,
            ja_start,
            (call_base + i) as i32 * 8,
        );

        let next_block_val = builder.ins().iconst(ptr_ty, after_arg_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next_block_val)]);

        branch_switch.set_entry(after_arg_id as u128, b);
        next_id = after_arg_id + 1;
    }

    if ctx.is_declared_rt_func_in_prog(callee_name) {
        let b = builder.create_block();
        builder.switch_to_block(b);

        // #81 — inline rt_load_u64 / rt_store (scheduler-trusted JUMP_ARGS).
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        let ja_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::JUMP_ARGS,
        );
        let ja_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ja_fat, 0);

        let mut arg_vals = Vec::with_capacity(args.len());
        for i in 0..args.len() {
            arg_vals.push(builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                ja_start,
                (call_base + i) as i32 * 8,
            ));
        }

        let func_ref = ctx.get_func(builder, callee_name)?;
        let call = builder.ins().call(func_ref, &arg_vals);

        // If the rt function returns a value, store it in TEMP_VAL.
        let ret_val = builder.inst_results(call).first().copied();
        if let Some(val) = ret_val {
            builder.ins().store(
                MemFlags::trusted(),
                val,
                exec_start,
                ExecCtxLayout::TEMP_VAL,
            );
        }

        // Park-aware rt fns: each one swaps the running actor out of
        // process_arr and into io_parked.  The call itself is emitted
        // normally above; here we override the post-call jump:
        //   1. Store the resume block id (next_id + 1) into ExecCtx.BLOCK_ID
        //      so the woken actor picks up where it left off.
        //   2. Jump to quantum_continue with STOP_PARK as the next-id
        //      marker; the dispatch chain turns that into a `return
        //      STOP_PARK` from the machine, which the scheduler interprets
        //      as "slot already vacated, just continue the loop".
        //
        // For rt_accept_async / rt_recv_async / similar, the rt fn body
        // submits the SQE itself; we still need to issue the park epilogue
        // here because the rt fn doesn't (can't) terminate the caller's
        // CPS block.  rt_io_park_current is the bare park primitive used
        // when the user paired it with a separate submit (rt_write_async).
        if matches!(
            *callee_name,
            "rt_io_park_current" | "rt_accept_async" | "rt_send_async" | "rt_recv_async"
        ) {
            let resume_id = builder.ins().iconst(ptr_ty, next_id + 1);
            let exec_start = ctx.exec_start(builder, machine_ctx_var);
            builder.ins().store(
                MemFlags::trusted(),
                resume_id,
                exec_start,
                ExecCtxLayout::BLOCK_ID,
            );
            let park_marker = builder.ins().iconst(ptr_ty, STOP_PARK);
            let qb = ctx.quantum_block();
            builder.ins().jump(qb, &[BlockArg::Value(park_marker)]);

            branch_switch.set_entry(next_id as u128, b);
            return Ok(StmtOutcome::Continue(next_id + 1));
        }

        let done = builder.ins().iconst(ptr_ty, next_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(done)]);

        branch_switch.set_entry(next_id as u128, b);
        Ok(StmtOutcome::Continue(next_id + 1))
    } else {
        // ── Check if callee is a pid-typed variable → message send ──────
        // After the resolver runs the AST has every variable's type
        // filled in, but a small minority (let-RHS that the inferrer
        // gave up on, plus expressions the resolver hasn't reached) can
        // still arrive here as `Type::Unknown` rendering as `?`.  In
        // that case we fall back to the BranchState's lake-type table,
        // which carries the type recorded at let / pattern binding
        // time.  Anything past that stays unknown.
        let callee_lake_type = {
            let raw = _ty.to_string();
            if raw == "?" {
                state.lake_type_of(callee_name).unwrap_or("?").to_string()
            } else {
                raw
            }
        };

        // FIXME(lake_frontend): the parser incorrectly resolves types for machine
        // names (e.g. `worker` gets raw_ty="pid"). We work around this by checking
        // state.get() to distinguish variables from machine names. Fix in lake_frontend parser.
        if callee_lake_type == "pid" && state.get(callee_name).is_some() {
            return send_expr::compile_send(
                ctx,
                builder,
                machine_ctx_var,
                next_id,
                branch_switch,
                state,
                callee_name,
                args.len(),
                call_base,
            );
        }

        let call_hash = hash_call_args(args, state.lake_types());
        if let Some(name) = ctx.get_current_machine()
            && *callee_name == "self"
        {
            change_state_expr::compile(
                ctx,
                builder,
                machine_ctx_var,
                next_id,
                branch_switch,
                &name,
                call_hash,
                call_base,
            )
        } else {
            spawn_expr::compile_spawn(
                ctx,
                builder,
                machine_ctx_var,
                next_id,
                branch_switch,
                callee_name,
                call_hash,
                call_base,
            )
        }
    }
}

/// Emit a single fused block for `self(arg0, arg1, ...)` when all args are pure.
///
/// #81 (extended) — fused rt-call with all-pure args.  Skips JUMP_ARGS
/// staging entirely: folds args inline, calls the rt-fn directly with
/// the folded Values, stores result (if any) to TEMP_VAL.
///
/// vs the normal jump_expr path:
///   - eliminates N "save arg to JUMP_ARGS[i]" CPS sub-blocks
///   - eliminates the qb roundtrip between each arg compile
///   - one Cranelift block total
///
/// Big win for stdlib helpers that do many rt-calls per branch:
///   - set_be32 has 4 rt_store calls → 4×9 = 36 CPS-blocks → 4 single-block fused calls
///   - process_block has 8 set_be32 + 8 be32 reads per block → multiplicative
///
/// Park-aware rt fns (rt_io_park_current / rt_accept_async / rt_send_async /
/// rt_recv_async) need special epilogue (BLOCK_ID store + STOP_PARK return)
/// — those bail to the slow path.
fn compile_fused_rt_call(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    callee_name: &str,
    args: &[Expr<'_>],
) -> Result<StmtOutcome> {
    // Park-aware rt fns need the BLOCK_ID + STOP_PARK epilogue — let
    // them go through the slow path which handles that.
    if matches!(
        callee_name,
        "rt_io_park_current" | "rt_accept_async" | "rt_send_async" | "rt_recv_async"
    ) {
        // Fall through to non-fused path by signalling caller via a re-call.
        // Since the slow path is what would have been taken absent this
        // check, return an Err here would be wrong — instead, run the
        // legacy code by inlining it here.  Simplest: bail to caller
        // via panic — actually just direct-call to legacy by replicating.
        return compile_legacy_rt_call(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            callee_name,
            args,
        );
    }

    let ptr_ty = ctx.module().target_config().pointer_type();

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Load exec_start + vars_start once for fold.
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let vars_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::VARIABLES,
    );
    let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);

    // Fold each arg as a Value.
    let arg_vals: Vec<_> = args
        .iter()
        .map(|arg| pure_expr::fold(arg, builder, ptr_ty, Some(vars_start), state))
        .collect();

    // Call the rt-fn directly.
    let func_ref = ctx.get_func(builder, callee_name)?;
    let call = builder.ins().call(func_ref, &arg_vals);

    // If the rt-fn returns a value, stash it in TEMP_VAL (let_expr's
    // slow path / arg-staging reads it from there).
    let ret_val = builder.inst_results(call).first().copied();
    if let Some(val) = ret_val {
        builder.ins().store(
            MemFlags::trusted(),
            val,
            exec_start,
            ExecCtxLayout::TEMP_VAL,
        );
    }

    let next_id_val = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id_val)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(block_id + 1))
}

/// Legacy slow path for rt-fns that need the park epilogue.
/// Duplicates the relevant portion of `compile` for park-aware calls
/// when fused path can't be used.
fn compile_legacy_rt_call(
    _ctx: &mut CompilerCtx,
    _builder: &mut FunctionBuilder,
    _machine_ctx_var: Variable,
    _block_id: i64,
    _branch_switch: &mut Switch,
    _state: &BranchState,
    _callee_name: &str,
    _args: &[Expr<'_>],
) -> Result<StmtOutcome> {
    // For now, just bail — the existing jump_expr `compile` will be
    // re-entered with these args.  This is a placeholder; in practice
    // park-aware rt-fns with all-pure args are rare (the actor's
    // pid is usually a Var or a let result).
    bail!(
        "park-aware rt-fn with all-pure args — not yet supported on fused path; rewrite to pin through a regular variable"
    );
}

/// Instead of the normal ~10-block staging pipeline (eval → TEMP_VAL → JUMP_ARGS
/// → copy to VARIABLES → set BRANCH_ID), this emits one block:
///   1. Load exec_start and vars_start once (inline, trusted)
///   2. Compute all args via `pure_expr::fold` (inline arithmetic + variable loads)
///   3. Write results directly to VARIABLES (bypassing TEMP_VAL and JUMP_ARGS)
///   4. Set BRANCH_ID
///   5. Jump to quantum_continue(0)
fn compile_fused_self_call(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    machine_name: &str,
    call_hash: u64,
    args: &[Expr<'_>],
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let candidates = ctx.branches_for_hash(machine_name, call_hash);
    anyhow::ensure!(
        !candidates.is_empty(),
        "No branch matching call hash {:#018x} in '{}'",
        call_hash,
        machine_name
    );

    let b = builder.create_block();
    builder.switch_to_block(b);

    // 1. Load exec_start and vars_start once
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let vars_fp = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::VARIABLES,
    );
    let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0);

    // 2. Compute ALL values first (before any stores).
    //    This is critical: self(acc2, acc1+acc2) must read the original acc2
    //    before overwriting vars[0].
    let values: Vec<_> = args
        .iter()
        .map(|arg| pure_expr::fold(arg, builder, ptr_ty, Some(vars_start), state))
        .collect();

    // 3. Write all values directly to VARIABLES
    for (i, val) in values.iter().enumerate() {
        builder
            .ins()
            .store(MemFlags::trusted(), *val, vars_start, i as i32 * 8);
    }

    // 4. Set BRANCH_ID — with guard dispatch if multiple branches share this hash.
    //    Skip the store when we KNOW we're staying in the same branch
    //    (single-candidate self-loop to current branch).
    let single_candidate = candidates.len() == 1;
    let same_branch = single_candidate && Some(candidates[0].branch_id) == ctx.current_branch_id();

    let branch_id_val = if !single_candidate {
        let disc_pos = dispatch::find_first_guard_pos(&candidates);
        let disc = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), vars_start, disc_pos as i32 * 8);
        let namespace = ctx.next_dispatch_id();
        Some(dispatch::emit_guard_select(
            ctx,
            builder,
            ptr_ty,
            &candidates,
            disc,
            namespace,
        )?)
    } else if !same_branch {
        Some(builder.ins().iconst(ptr_ty, candidates[0].branch_id as i64))
    } else {
        // BRANCH_ID unchanged — skip the store.
        None
    };

    if let Some(bid) = branch_id_val {
        builder.ins().store(
            MemFlags::trusted(),
            bid,
            exec_start,
            ExecCtxLayout::BRANCH_ID,
        );
    }

    // 5. Tail-self loop optimization: when self() loops back to the
    //    same branch, BRANCH_ID didn't change — we don't need
    //    machine_switch's "load BRANCH_ID + Switch" indirect dispatch.
    //    Store BLOCK_ID=0 (for STOP_LIMIT correctness on quantum
    //    exhaustion), dec quantum, brif zero → fast_yield, else →
    //    branch_entry_block.  Skips qb's 3 stop-code brifs +
    //    machine_switch's load + indirect Switch.
    //
    //    Concurrency preserved: each iter decrements quantum exactly
    //    once, same as the qb path.  When quantum hits 0 we store
    //    BLOCK_ID=0 and return STOP_LIMIT — scheduler resumes via
    //    branch_switch[0] correctly.  No latency regression: we
    //    yield no less frequently than before.
    if same_branch
        && let (Some(qv), Some(yb), Some(bb)) = (
            ctx.quantum_var(),
            ctx.yield_block(),
            ctx.current_branch_entry_block(),
        )
    {
        // BLOCK_ID = 0 (so re-entry after STOP_LIMIT lands at body start).
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder.ins().store(
            MemFlags::trusted(),
            zero,
            exec_start,
            ExecCtxLayout::BLOCK_ID,
        );

        let remaining = builder.use_var(qv);
        let new_remaining = builder.ins().iadd_imm(remaining, -1);
        builder.def_var(qv, new_remaining);
        let exhausted = builder
            .ins()
            .icmp_imm(cranelift::prelude::IntCC::Equal, new_remaining, 0);

        // brif → branch_entry (skips machine_switch).
        // branch_entry reloads var-cache + dispatches via branch_switch.
        builder
            .ins()
            .brif(exhausted, yb, &[BlockArg::Value(zero)], bb, &[]);
    } else {
        // Fallback: branch transition (different BRANCH_ID) or pre-Level-2
        // build — go through the full qb dispatch chain.
        let next_id = builder.ins().iconst(ptr_ty, 0);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next_id)]);
    }

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::StateChange {
        next_available: block_id + 1,
    })
}
