use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{FuncOrDataId, Module},
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome},
    rt::layout::{
        ExecCtxLayout, FatPtrLayout, process_ctx::ProcessCtxLayout,
        sheduler_ctx::ShedulerCtxLayout,
    },
};

/// Compile a message send: `pid_var(arg0, arg1, ...)`.
///
/// Arguments are already staged in the sender's JUMP_ARGS[call_base..call_base+N].
///
/// 1. Load PID from sender's VARIABLES.
/// 2. Load target's exec_ctx via PID → ProcessCtx → exec_ctx_fat_ptr.
/// 3. Enqueue args into target's mailbox ring buffer.
/// 4. Try to wake the target (move from wait_arr → process_arr if waiting).
pub fn compile_send(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    callee_name: &str,
    arg_count: usize,
    call_base: usize,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let (_, var_index) = state
        .get(callee_name)
        .ok_or_else(|| anyhow::anyhow!("Undefined pid variable '{callee_name}'"))?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Inlined fat-ptr derefs: scheduler-trusted memory, no bounds check.
    // Each pair of loads replaces a rt_load_u64 function call (~5-15 ns
    // saved per send).  For ping_pong-style benches that's 200k sends ×
    // 4-5 function calls each = ~5-10 ms total.
    let size8 = builder.ins().iconst(ptr_ty, 8);

    // ── 1. Load PID from sender's VARIABLES ─────────────────────────────
    let ctx_ptr = builder.use_var(machine_ctx_var);
    // Sender's ExecCtx fat-ptr -> ExecCtx start.
    let sender_exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    // VARIABLES fat-ptr (offset 24 in ExecCtx).
    let sender_vars_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        sender_exec_start,
        ExecCtxLayout::VARIABLES,
    );
    let vars_ptr = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), sender_vars_fat, 0);
    let pid_val = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        vars_ptr,
        var_index as i32 * 8,
    );
    let _ = rt_funcs;

    // ── 1a. Resolve pid → proc_ctx via the scheduler's pid_table.
    // The lowering pass for ret-machine calls without a `let` binding
    // passes 0 as the implicit __caller pid sentinel.  Slot 0 of the
    // pid_table is reserved as the null sentinel and stays 0, so the
    // null pid yields a 0 proc_ctx and we silently drop the send.
    // Dead actors' pids likewise read back as 0 because
    // `clear_pid` zeroes their slot during reclamation, so a stale
    // pid (#73) can no longer cause a wrong actor to receive the
    // message.
    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => bail!("sheduler_ctx_fat_ptr global not found"),
    };
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    let sh_fat_ptr_for_lookup = builder.ins().global_value(ptr_ty, sched_gv);
    let target_proc_ctx_fat = ShedulerCtxLayout::lookup_proc_ctx(
        sh_fat_ptr_for_lookup,
        pid_val,
        ctx,
        builder,
    );

    let send_block = builder.create_block();
    let continue_block = builder.create_block();
    let proc_ctx_is_null = builder
        .ins()
        .icmp_imm(IntCC::Equal, target_proc_ctx_fat, 0);
    builder
        .ins()
        .brif(proc_ctx_is_null, continue_block, &[], send_block, &[]);

    builder.switch_to_block(send_block);

    // ── 2. Load target's exec_ctx via its proc_ctx fat-ptr ──────────────
    let target_exec_ctx_fat = ProcessCtxLayout::get_exec_ctx(target_proc_ctx_fat, ctx, builder)?;
    let target_exec_start =
        FatPtrLayout::load_start(builder, ptr_ty, target_exec_ctx_fat);

    // ── 3. Enqueue args into target's mailbox ───────────────────────────
    let target_mailbox_fat = ExecCtxLayout::load(
        builder,
        ptr_ty,
        target_exec_start,
        ExecCtxLayout::MAILBOX_FAT,
    );
    let target_tail = ExecCtxLayout::load(
        builder,
        ptr_ty,
        target_exec_start,
        ExecCtxLayout::MAILBOX_TAIL,
    );

    // Load sender's JUMP_ARGS fat-ptr (inline deref, no rt call).
    let sender_ja_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        sender_exec_start,
        ExecCtxLayout::JUMP_ARGS,
    );
    let sender_ja = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), sender_ja_fat, 0);
    let target_mailbox_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), target_mailbox_fat, 0);

    // Copy each arg: JUMP_ARGS[call_base + i] → mailbox[(TAIL + i) mod 256]
    for i in 0..arg_count {
        let arg_val = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sender_ja,
            (call_base + i) as i32 * 8,
        );

        let msg_index = builder.ins().iadd_imm(target_tail, i as i64);
        let msg_index_mod = builder.ins().band_imm(msg_index, 255);
        let msg_offset = builder.ins().imul_imm(msg_index_mod, 8);
        let dst_addr = builder.ins().iadd(target_mailbox_start, msg_offset);
        builder
            .ins()
            .store(MemFlags::trusted(), arg_val, dst_addr, 0);
    }
    let _ = size8;

    // Update TAIL: (TAIL + arg_count) mod 256
    let new_tail = builder.ins().iadd_imm(target_tail, arg_count as i64);
    let new_tail_mod = builder.ins().band_imm(new_tail, 255);
    ExecCtxLayout::store(
        builder,
        new_tail_mod,
        target_exec_start,
        ExecCtxLayout::MAILBOX_TAIL,
    );

    // ── 4. Wake target process ──────────────────────────────────────────
    // Reuse the scheduler ctx fat-ptr we already loaded for the
    // pid_table lookup — same data section, no need to import twice.
    let sh_var = builder.declare_var(ptr_ty);
    builder.def_var(sh_var, sh_fat_ptr_for_lookup);

    ShedulerCtxLayout::wake_process(sh_var, pid_val, ctx, builder, continue_block)?;

    // ── Continue ────────────────────────────────────────────────────────
    builder.switch_to_block(continue_block);
    let next_id = block_id + 1;
    let next_id_val = builder.ins().iconst(ptr_ty, next_id);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id_val)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(next_id))
}
