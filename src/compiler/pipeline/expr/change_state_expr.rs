use anyhow::{Result, anyhow};
use crate::compiler::pipeline::expr::StmtOutcome;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{ExecCtxLayout, FatPtrLayout},
};

pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    machine_name: &str,
    call_hash: u64,
    jump_args_base: usize,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let (branch_id, _var_count, arg_count) = ctx
        .lookup_branch_by_hash(machine_name, call_hash)
        .ok_or_else(|| {
            anyhow!(
                "No branch matching call hash {:#018x} in '{}'",
                call_hash,
                machine_name
            )
        })?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

    let spawning_ctx_ptr = builder.use_var(machine_ctx_var);

    if arg_count > 0 {
        let vars_ptr_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
        let call_load_vars_fat_ptr = builder
            .ins()
            .call(load_ref, &[spawning_ctx_ptr, vars_ptr_offset]);
        let vars_fat_ptr = builder.inst_results(call_load_vars_fat_ptr)[0];
        let jump_args_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let call_ja = builder
            .ins()
            .call(load_ref, &[spawning_ctx_ptr, jump_args_offset]);
        let spawning_jump_args = builder.inst_results(call_ja)[0];
        let spawning_ja_start = FatPtrLayout::load_start(builder, ptr_ty, spawning_jump_args);

        let vars_start = FatPtrLayout::load_start(builder, ptr_ty, vars_fat_ptr);

        for i in 0..arg_count {
            let val = builder.ins().load(
                ptr_ty,
                MemFlags::new(),
                spawning_ja_start,
                (jump_args_base + i) as i32 * 8,
            );
            builder
                .ins()
                .store(MemFlags::new(), val, vars_start, i as i32 * 8);
        }
    }

    let branch_id_val = builder.ins().iconst(ptr_ty, branch_id as i64);
    let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);

    let branch_id_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::BRANCH_ID as i64);

    builder.ins().call(
        store_ref,
        &[spawning_ctx_ptr, branch_id_val, ptr_size, branch_id_offset],
    );

    let next_id = 0;
    let next_id_val = builder.ins().iconst(ptr_ty, next_id);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id_val)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::StateChange { next_available: block_id + 1 })
}
