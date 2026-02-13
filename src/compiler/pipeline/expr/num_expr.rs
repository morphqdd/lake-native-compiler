use anyhow::Result;
use cranelift::{
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::ExecCtxLayout};

/// Compile a numeric literal.
///
/// Places the value in `TEMP_VAL` and returns the next block id.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    num_str: &str,
) -> Result<i64> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let num: i64 = num_str.parse()?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    let num_val = builder.ins().iconst(ptr_ty, num);
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let store_ref = ctx.get_func(builder, "rt_store")?;
    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[ctx_ptr, num_val, size, temp_val_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, block_id + 1);
    builder.ins().return_(&[next_block_id]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(block_id + 1)
}
