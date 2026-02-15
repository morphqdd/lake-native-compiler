use anyhow::{Result, anyhow, bail};
use cranelift::{
    frontend::Switch,
    module::{FuncOrDataId, Module},
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{
        ExecCtxLayout, FatPtrLayout,
        process_ctx::ProcessCtxLayout,
        sheduler_ctx::ShedulerCtxLayout,
    },
};

/// Compile a spawn expression: allocate a new ExecCtx + ProcessCtx,
/// copy staged args from the spawning process's JUMP_ARGS into the spawned
/// process's VARIABLES (slots 0..arg_count-1), then register the new process
/// with the scheduler.
///
/// The spawning process continues: returns `block_id + 1`.
pub fn compile_spawn(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    machine_name: &str,
    arg_count: usize,
) -> Result<i64> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let branch_id = ctx
        .lookup_param_count(machine_name, arg_count)
        .ok_or_else(|| anyhow!("No branch with {} params in '{}'", arg_count, machine_name))?;
    let var_count = ctx
        .lookup_vars_count(machine_name, branch_id)
        .ok_or_else(|| anyhow!("Variable count for branch {} not found in '{}'", branch_id, machine_name))?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    let allocate_ref = rt_funcs.allocate_ref(ctx.module_mut(), builder);
    let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

    // ── Allocate ExecCtx (fat ptr layout: header[0..16] + data[16..]) ────────
    let exec_ctx_size = builder.ins().iconst(ptr_ty, ExecCtxLayout::SIZE as i64);
    let call_exec = builder.ins().call(allocate_ref, &[exec_ctx_size]);
    let exec_ctx_fat_ptr = builder.inst_results(call_exec)[0];
    // Raw data pointer for direct field stores.
    let exec_ctx_ptr = FatPtrLayout::load_start(builder, ptr_ty, exec_ctx_fat_ptr);

    // ── Allocate VARIABLES buffer ─────────────────────────────────────────────
    let vars_size = builder
        .ins()
        .iconst(ptr_ty, (var_count.max(1) * 8) as i64);
    let call_vars = builder.ins().call(allocate_ref, &[vars_size]);
    let vars_fat_ptr = builder.inst_results(call_vars)[0];

    // ── Allocate JUMP_ARGS buffer ─────────────────────────────────────────────
    let jump_args_size = builder.ins().iconst(ptr_ty, 256 * 8i64);
    let call_args = builder.ins().call(allocate_ref, &[jump_args_size]);
    let jump_args_fat_ptr = builder.inst_results(call_args)[0];

    // ── Copy staged args from spawning process JUMP_ARGS → spawned VARIABLES ──
    if arg_count > 0 {
        let spawning_ctx_ptr = builder.use_var(machine_ctx_var);
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
            let val = builder
                .ins()
                .load(ptr_ty, MemFlags::new(), spawning_ja_start, i as i32 * 8);
            builder
                .ins()
                .store(MemFlags::new(), val, vars_start, i as i32 * 8);
        }
    }

    // ── Initialise ExecCtx fields (raw stores into data region) ───────────────
    let branch_id_val = builder.ins().iconst(ptr_ty, branch_id as i64);
    let zero = builder.ins().iconst(ptr_ty, 0);
    ExecCtxLayout::store(builder, branch_id_val, exec_ctx_ptr, ExecCtxLayout::BRANCH_ID);
    ExecCtxLayout::store(builder, zero, exec_ctx_ptr, ExecCtxLayout::BLOCK_ID);
    ExecCtxLayout::store(builder, vars_fat_ptr, exec_ctx_ptr, ExecCtxLayout::VARIABLES);
    ExecCtxLayout::store(
        builder,
        jump_args_fat_ptr,
        exec_ctx_ptr,
        ExecCtxLayout::JUMP_ARGS,
    );

    // ── Create ProcessCtx ─────────────────────────────────────────────────────
    let proc_ctx_fat_ptr =
        ProcessCtxLayout::init_ctx(ctx, builder, machine_name, exec_ctx_fat_ptr)?;

    // ── Load scheduler fat ptr from global ────────────────────────────────────
    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => bail!("sheduler_ctx_fat_ptr global not found"),
    };
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    let sh_ctx_ptr = builder.ins().global_value(ptr_ty, sched_gv);

    // ── Register with scheduler ───────────────────────────────────────────────
    ShedulerCtxLayout::new_process(sh_ctx_ptr, proc_ctx_fat_ptr, ctx, builder)?;

    // Spawning process continues at the next block.
    let next_id = block_id + 1;
    let next_id_val = builder.ins().iconst(ptr_ty, next_id);
    builder.ins().return_(&[next_id_val]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(next_id)
}
