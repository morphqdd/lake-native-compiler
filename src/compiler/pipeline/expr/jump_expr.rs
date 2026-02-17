use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    hash_call_args,
    pipeline::expr::{BranchState, StmtOutcome, change_state_expr, compile_expr, spawn_expr},
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

        let done = builder.ins().iconst(ptr_ty, next_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(done)]);

        branch_switch.set_entry(next_id as u128, b);
        Ok(StmtOutcome::Continue(next_id + 1))
    } else {
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
