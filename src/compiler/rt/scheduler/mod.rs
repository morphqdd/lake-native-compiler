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
    module::Module,
    prelude::{AbiParam, FunctionBuilder, InstBuilder, IntCC, TrapCode},
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::machine::STOP_DONE,
    rt::layout::{process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout},
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

    // ── Blocks ────────────────────────────────────────────────────────────────
    let loop_block          = builder.create_block();
    let exec_block          = builder.create_block();
    let end_of_process_block = builder.create_block();
    let next_process_block  = builder.create_block();
    let exit_block          = builder.create_block();

    builder.ins().jump(loop_block, &[]);

    // ── loop_block: any processes left? ───────────────────────────────────────
    builder.switch_to_block(loop_block);
    let count = ShedulerCtxLayout::get_real_count_of_processes(sh_ptr_var, ctx, builder)?;
    let has_procs = builder.ins().icmp_imm(IntCC::NotEqual, count, 0);
    builder.ins().brif(has_procs, exec_block, &[], exit_block, &[]);

    // ── exec_block: call current process's machine (runs quantum internally) ──
    builder.switch_to_block(exec_block);
    let current = ShedulerCtxLayout::get_current_process(sh_ptr_var, ctx, builder)?;
    let func_addr = ProcessCtxLayout::get_func_addr(current, ctx, builder)?;
    let exec_ctx  = ProcessCtxLayout::get_exec_ctx(current, ctx, builder)?;

    let mut machine_sig = ctx.module().make_signature();
    machine_sig.params.push(AbiParam::new(ptr_ty));
    machine_sig.returns.push(AbiParam::new(ptr_ty));
    let sig_ref = builder.import_signature(machine_sig);

    let call      = builder.ins().call_indirect(sig_ref, func_addr, &[exec_ctx]);
    let stop_code = builder.inst_results(call)[0];

    // STOP_DONE (-1): process finished → remove it.
    // STOP_LIMIT (-2) or anything else: quantum done → round-robin.
    let is_done = builder.ins().icmp_imm(IntCC::Equal, stop_code, STOP_DONE);
    builder.ins().brif(is_done, end_of_process_block, &[], next_process_block, &[]);

    // ── end_of_process_block ──────────────────────────────────────────────────
    builder.switch_to_block(end_of_process_block);
    ShedulerCtxLayout::remove_current_process(sh_ptr_var, ctx, builder, loop_block)?;

    // ── next_process_block: round-robin ───────────────────────────────────────
    builder.switch_to_block(next_process_block);
    ShedulerCtxLayout::next_process(sh_ptr_var, ctx, builder, loop_block);

    // ── exit_block ────────────────────────────────────────────────────────────
    builder.switch_to_block(exit_block);
    let exit_ref = rt_funcs.exit_ref(ctx.module_mut(), builder);
    let zero = builder.ins().iconst(ptr_ty, 0);
    builder.ins().call(exit_ref, &[zero]);
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    Ok(())
}
