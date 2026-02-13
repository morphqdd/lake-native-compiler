use anyhow::{Result, bail};
use cranelift::{
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, compile_expr},
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
) -> Result<i64> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let Expr::Var(callee_name) = ident else {
        bail!("Jump target must be a variable/identifier");
    };

    // ── Compile each argument ────────────────────────────────────────────────
    let mut next_id = block_id;

    for (i, arg) in args.iter().enumerate() {
        // Compile the argument; result ends up in TEMP_VAL.
        let after_arg_id = compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            next_id,
            branch_switch,
            state,
            arg,
        )?;

        // Block: move TEMP_VAL → JUMP_ARGS[i].
        let b = builder.create_block();
        builder.switch_to_block(b);

        let ctx_ptr = builder.use_var(machine_ctx_var);
        let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
        let store_ref = ctx.get_func(builder, "rt_store")?;

        let temp_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let temp_call = builder.ins().call(load_u64_ref, &[ctx_ptr, temp_offset]);
        let arg_val = builder.inst_results(temp_call)[0];

        let jump_args_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let args_call = builder.ins().call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
        let args_ptr = builder.inst_results(args_call)[0];

        let slot_offset = builder.ins().iconst(ptr_ty, i as i64 * 8);
        let size = builder.ins().iconst(ptr_ty, 8);
        builder
            .ins()
            .call(store_ref, &[args_ptr, arg_val, size, slot_offset]);

        let next_block_val = builder.ins().iconst(ptr_ty, after_arg_id + 1);
        builder.ins().return_(&[next_block_val]);

        branch_switch.set_entry(after_arg_id as u128, b);
        next_id = after_arg_id + 1;
    }

    // ── Call block: load all args and call the target machine ────────────────
    let b = builder.create_block();
    builder.switch_to_block(b);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;

    let jump_args_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
    let args_call = builder.ins().call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
    let args_ptr = builder.inst_results(args_call)[0];

    let mut arg_vals = Vec::with_capacity(args.len());
    for i in 0..args.len() {
        let slot_offset = builder.ins().iconst(ptr_ty, i as i64 * 8);
        let val_call = builder.ins().call(load_u64_ref, &[args_ptr, slot_offset]);
        arg_vals.push(builder.inst_results(val_call)[0]);
    }

    let func_ref = ctx.get_func(builder, callee_name)?;
    builder.ins().call(func_ref, &arg_vals);

    // Return -1: this branch is complete; the scheduler should not re-enter it.
    let done = builder.ins().iconst(ptr_ty, -1);
    builder.ins().return_(&[done]);

    branch_switch.set_entry(next_id as u128, b);
    Ok(next_id + 1)
}
