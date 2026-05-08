use anyhow::Result;
// Scheduler infrastructure (Cranelift IR level).
//
// The scheduler drives the process queue.  Each machine function now runs its
// own inner quantum loop and returns a stop code:
//
//   STOP_DONE  (-1)  — process finished; remove from queue.
//   STOP_LIMIT (-2)  — quantum exhausted; BLOCK_ID already stored; round-robin.
//
// Future stop codes (STOP_WAIT etc.) will be added here as new variants.
use cranelift::{
    codegen::ir::BlockArg,
    module::Module,
    prelude::{AbiParam, FunctionBuilder, InstBuilder, IntCC, MemFlags, TrapCode},
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::machine::{STOP_DONE, STOP_PARK, STOP_WAIT},
    rt::layout::{process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout},
};

pub fn build_scheduler(ctx: &mut CompilerCtx, builder: &mut FunctionBuilder) -> Result<()> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let entry_block = builder.create_block();
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);

    let init_heap_ref = ctx.get_func(builder, "rt_init_heap")?;
    builder.ins().call(init_heap_ref, &[]);

    let sh_ptr_var = ShedulerCtxLayout::init(ctx, builder)?;

    // Set up io_uring rings BEFORE main process spawns — gives the scheduler
    // a working SQ/CQ before any actor can issue async I/O.
    let sh_ctx_fat = builder.use_var(sh_ptr_var);
    let io_setup_ref = ctx.get_func(builder, "rt_io_uring_setup")?;
    builder.ins().call(io_setup_ref, &[sh_ctx_fat]);

    ShedulerCtxLayout::init_main_process(sh_ptr_var, ctx, builder)?;

    let loop_block = builder.create_block();
    let exec_block = builder.create_block();
    let end_of_process_block = builder.create_block();
    let is_wait_block = builder.create_block();
    builder.append_block_param(is_wait_block, ptr_ty);
    let go_to_wait_block = builder.create_block();
    let next_process_block = builder.create_block();
    let is_park_block = builder.create_block();
    builder.append_block_param(is_park_block, ptr_ty);
    let exit_block = builder.create_block();

    builder.ins().jump(loop_block, &[]);

    let check_waited_block = builder.create_block();

    builder.switch_to_block(loop_block);
    // Drain CQ ring on each scheduler tick: wakes any actor whose I/O
    // completed since last iteration, re-adding them to process_arr.  Cheap
    // when ring is empty (one head==tail compare and bail).
    let poll_cq_ref = ctx.get_func(builder, "rt_io_uring_poll_cq")?;
    builder.ins().call(poll_cq_ref, &[]);

    let real_count = ShedulerCtxLayout::get_real_count_of_processes(sh_ptr_var, ctx, builder)?;
    let has_active = builder.ins().icmp_imm(IntCC::NotEqual, real_count, 0);
    builder
        .ins()
        .brif(has_active, exec_block, &[], check_waited_block, &[]);

    builder.switch_to_block(check_waited_block);
    let waited = ShedulerCtxLayout::get_waited_processes(sh_ptr_var, ctx, builder)?;
    let has_waited = builder.ins().icmp_imm(IntCC::NotEqual, waited, 0);
    let check_io_parked_block = builder.create_block();
    builder
        .ins()
        .brif(has_waited, loop_block, &[], check_io_parked_block, &[]);

    // No runnable processes and no message-waiters — but if anyone is parked
    // on I/O, block on `io_uring_enter(min_complete=1)` until at least one
    // CQE arrives, then loop back and let `poll_cq` wake the lucky actor(s).
    builder.switch_to_block(check_io_parked_block);
    builder.seal_block(check_io_parked_block);
    let sh_use = builder.use_var(sh_ptr_var);
    // sh_use is the fat-ptr address; deref to get raw sh_ctx data start.
    let sh_data = builder.ins().load(ptr_ty, MemFlags::trusted(), sh_use, 0);
    let parked_count = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        sh_data,
        ShedulerCtxLayout::IO_PARKED_COUNT,
    );
    let has_parked = builder.ins().icmp_imm(IntCC::NotEqual, parked_count, 0);
    let wait_io_block = builder.create_block();
    builder
        .ins()
        .brif(has_parked, wait_io_block, &[], exit_block, &[]);

    builder.switch_to_block(wait_io_block);
    builder.seal_block(wait_io_block);
    // Flush any residual pending submissions first so the kernel actually
    // has work to complete.
    let flush_ref0 = ctx.get_func(builder, "rt_io_uring_flush")?;
    builder.ins().call(flush_ref0, &[]);
    let wait_ref = ctx.get_func(builder, "rt_io_uring_wait_cqe")?;
    builder.ins().call(wait_ref, &[]);
    builder.ins().jump(loop_block, &[]);

    builder.switch_to_block(exec_block);
    let current = ShedulerCtxLayout::get_current_process(sh_ptr_var, ctx, builder)?;
    let func_addr = ProcessCtxLayout::get_func_addr(current, ctx, builder)?;
    let exec_ctx = ProcessCtxLayout::get_exec_ctx(current, ctx, builder)?;

    let mut machine_sig = ctx.module().make_signature();
    machine_sig.params.push(AbiParam::new(ptr_ty));
    machine_sig.returns.push(AbiParam::new(ptr_ty));
    let sig_ref = builder.import_signature(machine_sig);

    let call = builder.ins().call_indirect(sig_ref, func_addr, &[exec_ctx]);
    let stop_code = builder.inst_results(call)[0];

    let is_done = builder.ins().icmp_imm(IntCC::Equal, stop_code, STOP_DONE);
    builder.ins().brif(
        is_done,
        end_of_process_block,
        &[],
        is_wait_block,
        &[BlockArg::Value(stop_code)],
    );

    builder.switch_to_block(end_of_process_block);
    // Reclaim the heap memory owned by the process before unlinking from
    // the queue (so we don't lose the fat-ptr handles).
    let dead_process = ShedulerCtxLayout::get_current_process(sh_ptr_var, ctx, builder)?;
    ShedulerCtxLayout::free_process_resources(dead_process, ctx, builder);
    ShedulerCtxLayout::remove_current_process(sh_ptr_var, ctx, builder, loop_block)?;

    builder.switch_to_block(is_wait_block);
    let stop_code = builder.block_params(is_wait_block)[0];
    let is_wait = builder.ins().icmp_imm(IntCC::Equal, stop_code, STOP_WAIT);
    builder.ins().brif(
        is_wait,
        go_to_wait_block,
        &[],
        is_park_block,
        &[BlockArg::Value(stop_code)],
    );

    builder.switch_to_block(is_park_block);
    let stop_code = builder.block_params(is_park_block)[0];
    let is_park = builder.ins().icmp_imm(IntCC::Equal, stop_code, STOP_PARK);
    // Parked: slot already vacated by rt_io_park_current, BLOCK_ID already
    // points at the resume location.  Just loop — scheduler picks the next
    // active actor.  Otherwise (none of the special codes), advance via
    // next_process_block.
    builder
        .ins()
        .brif(is_park, loop_block, &[], next_process_block, &[]);

    builder.switch_to_block(go_to_wait_block);
    let process = ShedulerCtxLayout::get_current_process(sh_ptr_var, ctx, builder)?;
    ShedulerCtxLayout::wait_current_process(sh_ptr_var, process, ctx, builder)?;
    ShedulerCtxLayout::remove_current_process(sh_ptr_var, ctx, builder, loop_block)?;

    builder.switch_to_block(next_process_block);
    ShedulerCtxLayout::next_process(sh_ptr_var, ctx, builder, loop_block);

    builder.switch_to_block(exit_block);
    // Flush any residual SQEs queued by `rt_write_async` so a partial batch
    // (count < SQE_BATCH_SIZE) doesn't get dropped when the kernel closes
    // the ring fd at exit.
    let flush_ref = ctx.get_func(builder, "rt_io_uring_flush")?;
    builder.ins().call(flush_ref, &[]);
    let exit_ref = rt_funcs.exit_ref(ctx.module_mut(), builder);
    let zero = builder.ins().iconst(ptr_ty, 0);
    builder.ins().call(exit_ref, &[zero]);
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    Ok(())
}
