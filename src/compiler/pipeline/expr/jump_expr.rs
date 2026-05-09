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

        let ctx_ptr = builder.use_var(machine_ctx_var);
        let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);

        let temp_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let temp_call = builder.ins().call(load_u64_ref, &[ctx_ptr, temp_offset]);
        let arg_val = builder.inst_results(temp_call)[0];

        let jump_args_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let args_call = builder
            .ins()
            .call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
        let args_ptr = builder.inst_results(args_call)[0];

        let slot_offset = builder.ins().iconst(ptr_ty, (call_base + i) as i64 * 8);
        let size = builder.ins().iconst(ptr_ty, 8);
        builder
            .ins()
            .call(store_ref, &[args_ptr, arg_val, size, slot_offset]);

        let next_block_val = builder.ins().iconst(ptr_ty, after_arg_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next_block_val)]);

        branch_switch.set_entry(after_arg_id as u128, b);
        next_id = after_arg_id + 1;
    }

    if ctx.is_declared_rt_func_in_prog(callee_name) {
        let b = builder.create_block();
        builder.switch_to_block(b);

        let ctx_ptr = builder.use_var(machine_ctx_var);
        let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

        let jump_args_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let args_call = builder
            .ins()
            .call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
        let args_ptr = builder.inst_results(args_call)[0];

        let mut arg_vals = Vec::with_capacity(args.len());
        for i in 0..args.len() {
            let slot_offset = builder.ins().iconst(ptr_ty, (call_base + i) as i64 * 8);
            let val_call = builder.ins().call(load_u64_ref, &[args_ptr, slot_offset]);
            arg_vals.push(builder.inst_results(val_call)[0]);
        }

        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let func_ref = ctx.get_func(builder, callee_name)?;
        let call = builder.ins().call(func_ref, &arg_vals);

        // If the rt function returns a value, store it in TEMP_VAL so that
        // the caller can stage it as an argument for a subsequent spawn.
        let ret_val = builder.inst_results(call).first().copied();
        if let Some(val) = ret_val {
            let ctx_ptr = builder.use_var(machine_ctx_var);
            let temp_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
            let size = builder.ins().iconst(ptr_ty, 8);
            builder
                .ins()
                .call(store_ref, &[ctx_ptr, val, size, temp_offset]);
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
            "rt_io_park_current" | "rt_accept_async" | "rt_send_async"
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
                state
                    .lake_type_of(callee_name)
                    .unwrap_or("?")
                    .to_string()
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
    let vars_fp = builder.ins().load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::VARIABLES);
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
        builder.ins().store(MemFlags::trusted(), *val, vars_start, i as i32 * 8);
    }

    // 4. Set BRANCH_ID — with guard dispatch if multiple branches share this hash
    let branch_id_val = if candidates.len() > 1 {
        let disc_pos = dispatch::find_first_guard_pos(&candidates);
        // Discriminant is the already-computed arg value at disc_pos.
        // We read it back from VARIABLES (which we just wrote) — safe because
        // writes happened before this load in program order.
        let disc = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            vars_start,
            disc_pos as i32 * 8,
        );
        let namespace = ctx.next_dispatch_id();
        dispatch::emit_guard_select(ctx, builder, ptr_ty, &candidates, disc, namespace)?
    } else {
        builder.ins().iconst(ptr_ty, candidates[0].branch_id as i64)
    };

    // exec_start was computed in block b; b_merge is dominated by b, so this is valid.
    builder.ins().store(MemFlags::trusted(), branch_id_val, exec_start, ExecCtxLayout::BRANCH_ID);

    // 5. Jump to quantum_continue with block_id = 0
    let next_id = builder.ins().iconst(ptr_ty, 0);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::StateChange {
        next_available: block_id + 1,
    })
}
