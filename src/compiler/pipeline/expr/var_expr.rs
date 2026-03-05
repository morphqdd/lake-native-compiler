use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome},
    rt::layout::ExecCtxLayout,
};

/// Compile a variable read.
///
/// Loads `vars[slot]` into `TEMP_VAL` so the next expression can consume it.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    var_name: &str,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let (_, var_index) = state
        .get(var_name)
        .ok_or_else(|| anyhow::anyhow!("Undefined variable '{var_name}'"))?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
    let store_ref = ctx.get_func(builder, "rt_store")?;

    // Load the variables fat ptr.
    let vars_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let vars_call = builder.ins().call(load_u64_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(vars_call)[0];

    // Load the value at vars[var_index].
    let var_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
    let val_call = builder
        .ins()
        .call(load_u64_ref, &[vars_ptr, var_offset]);
    let val = builder.inst_results(val_call)[0];

    // Store it in TEMP_VAL.
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let temp_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[ctx_ptr, val, size, temp_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_block_id)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(block_id + 1))
}
