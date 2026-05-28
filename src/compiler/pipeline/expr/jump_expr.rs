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
    rt::layout::{ExecCtxLayout, process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout},
};
use cranelift::module::FuncOrDataId;

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
            let arg_types = crate::compiler::expr_type_strs(args, state.lake_types());
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
                &arg_types,
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
            | "rt_pidfd_poll_async"
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
            | "rt_pidfd_poll_async"
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
        let arg_types = crate::compiler::expr_type_strs(args, state.lake_types());
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
                &arg_types,
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
                &arg_types,
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
            | "rt_pidfd_poll_async"
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
    arg_types: &[String],
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

    // 2.5 (bug #152 latent): for own-arena actors (non-ret machines)
    // with pointer-typed args, the arena reset in step 3.5 would
    // invalidate the pointer payloads on the next iteration's first
    // allocation.  Snapshot ptr args to a scratch buf, reset arena,
    // re-alloc + recopy into the fresh arena.  Mirrors the slow
    // path (`change_state_expr.rs:179-295`).
    //
    // Skipped for sync-ret-machines (inherited arena) because the
    // arena reset in step 3.5 is a no-op (gated on
    // `OWNED_ARENA_BASE != 0`), so the original ptr args stay
    // valid through the self() iteration.  Doing snapshot+recopy
    // there would bump the inherited caller's arena every iter,
    // amplifying allocations for stdlib helpers like
    // `next_newline`, `slice_buf`, etc.
    let mut final_values = values.clone();
    let is_own_arena = !ctx.is_ret_machine(machine_name);
    let ptr_arg_idxs: Vec<usize> = if is_own_arena {
        arg_types
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                if crate::compiler::is_pointer_like_type(t) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    if !ptr_arg_idxs.is_empty() {
        // Compute per-arg sizes (end - start) and total.
        let mut ptr_arg_sizes: Vec<(usize, cranelift::prelude::Value)> = Vec::new();
        let mut total_size_opt: Option<cranelift::prelude::Value> = None;
        for &i in &ptr_arg_idxs {
            let fp = values[i];
            let start = builder.ins().load(ptr_ty, MemFlags::trusted(), fp, 0);
            let end = builder.ins().load(ptr_ty, MemFlags::trusted(), fp, 8);
            let sz = builder.ins().isub(end, start);
            total_size_opt = Some(match total_size_opt {
                None => sz,
                Some(prev) => builder.ins().iadd(prev, sz),
            });
            ptr_arg_sizes.push((i, sz));
        }

        let alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => bail!("rt_allocate_raw not declared"),
        };
        let alloc_raw_ref = ctx
            .module_mut()
            .declare_func_in_func(alloc_raw_id, &mut builder.func);
        let copy_bytes_id = match ctx.module().get_name("rt_copy_bytes") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => bail!("rt_copy_bytes not declared"),
        };
        let copy_bytes_ref = ctx
            .module_mut()
            .declare_func_in_func(copy_bytes_id, &mut builder.func);
        let free_id = match ctx.module().get_name("rt_free") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => bail!("rt_free not declared"),
        };
        let free_ref = ctx
            .module_mut()
            .declare_func_in_func(free_id, &mut builder.func);
        let arena_alloc_id = match ctx.module().get_name("rt_arena_alloc") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => bail!("rt_arena_alloc not declared"),
        };
        let arena_alloc_ref = ctx
            .module_mut()
            .declare_func_in_func(arena_alloc_id, &mut builder.func);

        let total_size = total_size_opt.unwrap();
        let call_scratch = builder.ins().call(alloc_raw_ref, &[total_size]);
        let scratch_fat = builder.inst_results(call_scratch)[0];
        let zero_off = builder.ins().iconst(ptr_ty, 0);

        // Snapshot each ptr-arg's payload bytes into the scratch buf.
        let mut offsets: Vec<(usize, cranelift::prelude::Value, cranelift::prelude::Value)> =
            Vec::new();
        let mut cursor = builder.ins().iconst(ptr_ty, 0);
        for &(i, sz) in &ptr_arg_sizes {
            let src_fat = values[i];
            builder.ins().call(
                copy_bytes_ref,
                &[scratch_fat, cursor, src_fat, zero_off, sz],
            );
            offsets.push((i, cursor, sz));
            cursor = builder.ins().iadd(cursor, sz);
        }

        // Reset will fire in step 3.5 below; re-alloc each ptr arg
        // afterwards.  We rely on step 3.5 to actually reset the
        // arena before our re-allocs; since both run in the same
        // straight-line block, ordering is preserved.

        // (Tail of dance is emitted AFTER step 3.5 reset block.)
        // Stash refs and indices for the post-reset loop via a
        // closure-like pattern: just remember and re-iterate.

        // Emit step 3.5 reset inline here so we control ordering:
        // 1. snapshot (above)
        // 2. arena reset
        // 3. re-alloc + recopy (below)
        // 4. free scratch
        if let Some(FuncOrDataId::Data(sid)) = ctx.module().get_name("sheduler_ctx_fat_ptr") {
            let sched_gv = ctx
                .module_mut()
                .declare_data_in_func(sid, &mut builder.func);
            let sched_fat = builder.ins().global_value(ptr_ty, sched_gv);
            let sched_start = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sched_fat, 0);
            let cur_idx = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::CURRENT_PROCESS,
            );
            let proc_arr_fat = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::PROCESS_ARR_FAT,
            );
            let proc_arr = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), proc_arr_fat, 0);
            let cur_off = builder.ins().imul_imm(cur_idx, 8);
            let cur_entry_addr = builder.ins().iadd(proc_arr, cur_off);
            let proc_ctx_fat_ptr = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), cur_entry_addr, 0);
            let proc_ctx_start = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), proc_ctx_fat_ptr, 0);
            let arena_fat = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                proc_ctx_start,
                ProcessCtxLayout::OWNED_ARENA_FAT,
            );
            let arena_base = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                proc_ctx_start,
                ProcessCtxLayout::OWNED_ARENA_BASE,
            );
            // Reset only if we own (guard preserved even though
            // is_own_arena is true at compile time — runtime check
            // mirrors slow path's exact semantics).
            let owns = builder.ins().icmp_imm(
                cranelift::prelude::IntCC::NotEqual,
                arena_base,
                0,
            );
            let reset_blk = builder.create_block();
            let skip_blk = builder.create_block();
            builder.ins().brif(owns, reset_blk, &[], skip_blk, &[]);
            builder.switch_to_block(reset_blk);
            builder.seal_block(reset_blk);
            builder
                .ins()
                .store(MemFlags::trusted(), arena_base, arena_fat, 0);
            builder.ins().jump(skip_blk, &[]);
            builder.switch_to_block(skip_blk);
            builder.seal_block(skip_blk);
        }

        // Re-alloc each ptr arg in the (just-reset) arena, copy
        // payload bytes back from the scratch snapshot.
        for (i, off, sz) in &offsets {
            let call_dst = builder.ins().call(arena_alloc_ref, &[*sz]);
            let dst_fat = builder.inst_results(call_dst)[0];
            builder.ins().call(
                copy_bytes_ref,
                &[dst_fat, zero_off, scratch_fat, *off, *sz],
            );
            final_values[*i] = dst_fat;
        }

        // Free scratch.
        builder.ins().call(free_ref, &[scratch_fat]);

        // 3. Write final values directly to VARIABLES (post-recopy).
        for (i, val) in final_values.iter().enumerate() {
            builder
                .ins()
                .store(MemFlags::trusted(), *val, vars_start, i as i32 * 8);
        }
    } else {
        // No ptr args (or sync-ret machine — inherited arena).
        // Original simple flow: write vals to vars + reset arena
        // (which is a no-op when arena_base == 0).
        for (i, val) in values.iter().enumerate() {
            builder
                .ins()
                .store(MemFlags::trusted(), *val, vars_start, i as i32 * 8);
        }

        // Reset arena only when owned (#152 fix preserved for the
        // no-ptr-arg case — handles e.g. server_min's `self(fd)`).
        if let Some(FuncOrDataId::Data(sid)) = ctx.module().get_name("sheduler_ctx_fat_ptr") {
        let sched_gv = ctx.module_mut().declare_data_in_func(sid, &mut builder.func);
        let sched_fat = builder.ins().global_value(ptr_ty, sched_gv);
        let sched_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sched_fat, 0);
        let cur_idx = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sched_start,
            ShedulerCtxLayout::CURRENT_PROCESS,
        );
        let proc_arr_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sched_start,
            ShedulerCtxLayout::PROCESS_ARR_FAT,
        );
        let proc_arr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), proc_arr_fat, 0);
        let cur_off = builder.ins().imul_imm(cur_idx, 8);
        let cur_entry_addr = builder.ins().iadd(proc_arr, cur_off);
        let proc_ctx_fat_ptr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), cur_entry_addr, 0);
        let proc_ctx_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), proc_ctx_fat_ptr, 0);
        let arena_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_FAT,
        );
        let arena_base = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_BASE,
        );
        let owns = builder.ins().icmp_imm(
            cranelift::prelude::IntCC::NotEqual,
            arena_base,
            0,
        );
        let reset_blk = builder.create_block();
        let skip_blk = builder.create_block();
        builder.ins().brif(owns, reset_blk, &[], skip_blk, &[]);
        builder.switch_to_block(reset_blk);
        builder.seal_block(reset_blk);
        builder
            .ins()
            .store(MemFlags::trusted(), arena_base, arena_fat, 0);
        builder.ins().jump(skip_blk, &[]);
        builder.switch_to_block(skip_blk);
        builder.seal_block(skip_blk);
        }
    }

    // 4. Set BRANCH_ID — with guard dispatch if multiple branches share this hash.
    //    Skip the store when we KNOW we're staying in the same branch
    //    (single-candidate self-loop to current branch).
    let single_candidate = candidates.len() == 1;
    let same_branch = single_candidate && Some(candidates[0].branch_id) == ctx.current_branch_id();

    let branch_id_val = if !single_candidate {
        let disc_pos = dispatch::find_best_guard_pos(&candidates);
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
