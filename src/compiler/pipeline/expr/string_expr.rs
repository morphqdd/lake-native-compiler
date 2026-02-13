use std::hash::{DefaultHasher, Hash, Hasher};

use anyhow::Result;
use base64ct::{Base64, Encoding};
use cranelift::{
    frontend::Switch,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::ExecCtxLayout};

/// Compile a string literal `str."..."`.
///
/// The string bytes are placed in a `.rodata`-equivalent global data section.
/// A companion fat-pointer global (16 bytes: `[start, end]`) is initialised
/// at runtime to point at those bytes.  The fat-pointer address is stored in
/// `TEMP_VAL` so the next block can pick it up.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    s: &str,
) -> Result<i64> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // Deduplicate strings by hashing their contents.
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    let hash = hasher.finish();
    let encoded = Base64::encode_string(&hash.to_be_bytes());

    // Declare (or reuse) the raw string data.
    let data_id = match ctx.module().get_name(&encoded) {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            let id = ctx
                .module_mut()
                .declare_data(&encoded, Linkage::Export, false, false)?;
            let mut desc = DataDescription::new();
            desc.define(s.as_bytes().to_vec().into_boxed_slice());
            ctx.module_mut().define_data(id, &desc)?;
            id
        }
    };

    // Declare (or reuse) the fat-pointer companion.
    let fat_ptr_name = format!("fp_{encoded}");
    let fat_ptr_id = match ctx.module().get_name(&fat_ptr_name) {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            let id = ctx
                .module_mut()
                .declare_data(&fat_ptr_name, Linkage::Export, true, false)?;
            let mut desc = DataDescription::new();
            desc.define_zeroinit(16);
            ctx.module_mut().define_data(id, &desc)?;
            id
        }
    };

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Resolve global values for this function.
    let data_gv = ctx
        .module_mut()
        .declare_data_in_func(data_id, builder.func);
    let fat_ptr_gv = ctx
        .module_mut()
        .declare_data_in_func(fat_ptr_id, builder.func);

    let data_ptr = builder.ins().global_value(ptr_ty, data_gv);
    let fat_ptr = builder.ins().global_value(ptr_ty, fat_ptr_gv);
    let end_ptr = builder
        .ins()
        .iadd_imm(data_ptr, s.as_bytes().len() as i64);

    // Write [start, end] into the fat pointer.
    builder.ins().store(MemFlags::new(), data_ptr, fat_ptr, 0);
    builder.ins().store(MemFlags::new(), end_ptr, fat_ptr, 8);

    // Store the fat-pointer address in TEMP_VAL.
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let store_ref = ctx.get_func(builder, "rt_store")?;
    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[ctx_ptr, fat_ptr, size, temp_val_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, block_id + 1);
    builder.ins().return_(&[next_block_id]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(block_id + 1)
}
