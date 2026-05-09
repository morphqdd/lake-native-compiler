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

    let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
    let size8 = builder.ins().iconst(ptr_ty, 8);

    // ── 1. Load PID from sender's VARIABLES ─────────────────────────────
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let vars_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let call_vars = builder.ins().call(load_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(call_vars)[0];

    let pid_slot_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
    let call_pid = builder
        .ins()
        .call(load_ref, &[vars_ptr, pid_slot_offset]);
    let pid_val = builder.inst_results(call_pid)[0];

    // ── 1a. Null-pid check ──────────────────────────────────────────────
    // The lowering pass for ret-machine calls without a `let` binding
    // passes 0 as the implicit __caller pid sentinel.  When the
    // ret-machine then runs `__caller(self X)` (= a send to that null
    // pid), we silently drop it: dereferencing 0 as a process_ctx
    // fat-ptr would segfault.  Live pids are heap-allocated and never
    // zero, so a single icmp is sufficient.
    let send_block = builder.create_block();
    let continue_block = builder.create_block();
    let pid_is_null = builder.ins().icmp_imm(IntCC::Equal, pid_val, 0);
    builder
        .ins()
        .brif(pid_is_null, continue_block, &[], send_block, &[]);

    builder.switch_to_block(send_block);

    // ── 2. Load target's exec_ctx ───────────────────────────────────────
    // PID = process_ctx_fat_ptr
    let target_exec_ctx_fat = ProcessCtxLayout::get_exec_ctx(pid_val, ctx, builder)?;
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

    // Load sender's JUMP_ARGS
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let ja_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
    let call_ja = builder.ins().call(load_ref, &[ctx_ptr, ja_offset]);
    let sender_ja = builder.inst_results(call_ja)[0];

    // Copy each arg: JUMP_ARGS[call_base + i] → mailbox[(TAIL + i) mod 256]
    for i in 0..arg_count {
        let src_offset = builder.ins().iconst(ptr_ty, (call_base + i) as i64 * 8);
        let call_load_arg = builder.ins().call(load_ref, &[sender_ja, src_offset]);
        let arg_val = builder.inst_results(call_load_arg)[0];

        let msg_index = builder.ins().iadd_imm(target_tail, i as i64);
        let msg_index_mod = builder.ins().band_imm(msg_index, 255);
        let msg_offset = builder.ins().imul_imm(msg_index_mod, 8);
        builder
            .ins()
            .call(store_ref, &[target_mailbox_fat, arg_val, size8, msg_offset]);
    }

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
    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => bail!("sheduler_ctx_fat_ptr global not found"),
    };
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    let sh_fat_ptr = builder.ins().global_value(ptr_ty, sched_gv);

    // We need a Variable for wake_process. Create a temporary one.
    let sh_var = builder.declare_var(ptr_ty);
    builder.def_var(sh_var, sh_fat_ptr);

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
