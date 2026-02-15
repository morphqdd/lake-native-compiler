use anyhow::Result;
// Scheduler infrastructure (Cranelift IR level).
//
// The scheduler is the entity that:
//   1. Maintains a queue of active processes (each process = machine fn + ExecCtx).
//   2. Calls each machine function with its context for one block of work.
//   3. Stores the returned next-block-id back into the context.
//   4. Removes processes that return -1 (finished).
//   5. Loops until the queue is empty, then calls rt_exit(0).
use cranelift::{
    codegen::ir::BlockArg,
    module::Module,
    prelude::{AbiParam, FunctionBuilder, InstBuilder, IntCC, TrapCode},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{ExecCtxLayout, process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout},
};

pub fn build_scheduler(ctx: &mut CompilerCtx, builder: &mut FunctionBuilder) -> Result<()> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let entry_block = builder.create_block();
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);

    // Initialise the heap before any allocations.
    let init_heap_ref = ctx.get_func(builder, "rt_init_heap")?;
    builder.ins().call(init_heap_ref, &[]);

    let sh_ptr_var = ShedulerCtxLayout::init(ctx, builder)?;
    ShedulerCtxLayout::init_main_process(sh_ptr_var, ctx, builder)?;

    let loop_block = builder.create_block();
    let limit_check_block = builder.create_block();
    let exec_block = builder.create_block();
    let exit_block = builder.create_block();
    let continue_block = builder.create_block();
    let next_process_block = builder.create_block();
    let end_of_process_block = builder.create_block();
    builder.append_block_param(continue_block, ptr_ty); // next_block_id
    builder.append_block_param(continue_block, ptr_ty); // exec_ctx 

    builder.ins().jump(loop_block, &[]);
    builder.switch_to_block(loop_block);

    let real_count_of_processes =
        ShedulerCtxLayout::get_real_count_of_processes(sh_ptr_var, ctx, builder)?;
    let is_continue = builder
        .ins()
        .icmp_imm(IntCC::NotEqual, real_count_of_processes, 0);
    builder
        .ins()
        .brif(is_continue, limit_check_block, &[], exit_block, &[]);

    builder.switch_to_block(limit_check_block);
    let counter = ShedulerCtxLayout::get_reduction_counter(sh_ptr_var, ctx, builder)?;
    let limit = ShedulerCtxLayout::get_reduction_limit(sh_ptr_var, ctx, builder)?;

    let is_limit_reached = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, counter, limit);
    builder
        .ins()
        .brif(is_limit_reached, next_process_block, &[], exec_block, &[]);

    builder.switch_to_block(exec_block);
    let current_process_ctx = ShedulerCtxLayout::get_current_process(sh_ptr_var, ctx, builder)?;
    let func_addr = ProcessCtxLayout::get_func_addr(current_process_ctx, ctx, builder)?;
    let exec_ctx = ProcessCtxLayout::get_exec_ctx(current_process_ctx, ctx, builder)?;
    let mut default_machine_sig = ctx.module().make_signature();
    default_machine_sig.params.push(AbiParam::new(ptr_ty));
    default_machine_sig.returns.push(AbiParam::new(ptr_ty));
    let default_sig_ref = builder.import_signature(default_machine_sig);

    let call = builder
        .ins()
        .call_indirect(default_sig_ref, func_addr, &[exec_ctx]);
    let next_block_id = builder.inst_results(call)[0];
    let is_done = builder.ins().icmp_imm(IntCC::Equal, next_block_id, -1);
    builder.ins().brif(
        is_done,
        end_of_process_block,
        &[],
        continue_block,
        &[BlockArg::Value(next_block_id), BlockArg::Value(exec_ctx)],
    );

    builder.switch_to_block(continue_block);
    let next_id = builder.block_params(continue_block)[0];
    let exec_ctx = builder.block_params(continue_block)[1];
    ExecCtxLayout::set_next_block(exec_ctx, next_id, ctx, builder);
    ShedulerCtxLayout::increment_reduction_counter(sh_ptr_var, ctx, builder);
    builder.ins().jump(loop_block, &[]);

    builder.switch_to_block(end_of_process_block);
    ShedulerCtxLayout::remove_current_process(sh_ptr_var, ctx, builder, loop_block)?;

    builder.switch_to_block(next_process_block);
    ShedulerCtxLayout::next_process(sh_ptr_var, ctx, builder, loop_block);

    builder.switch_to_block(exit_block);
    let exit_ref = rt_funcs.exit_ref(ctx.module_mut(), builder);
    let zero = builder.ins().iconst(ptr_ty, 0);
    builder.ins().call(exit_ref, &[zero]);
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    Ok(())
}
