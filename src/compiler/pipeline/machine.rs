use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags},
};
use lake_frontend::api::ast::{Machine, MachineItem};
use log::{debug, trace};

use crate::compiler::{
    ctx::CompilerCtx, pipeline::branch::compile_branch, rt::layout::ExecCtxLayout,
};

/// Stop codes returned by a compiled machine function to the scheduler.
pub const STOP_DONE: i64 = -1; // process finished (no matching branch or explicit -1)
pub const STOP_LIMIT: i64 = -2; // quantum exhausted; BLOCK_ID already stored in exec_ctx

/// Compile a single Lake machine to a Cranelift function.
///
/// Signature: `fn(ctx_fat_ptr: ptr) -> stop_code: ptr`
///
/// The machine runs an inner quantum loop: it executes up to `quantum` CPS
/// blocks per scheduler call, storing the next BLOCK_ID into exec_ctx before
/// returning STOP_LIMIT.  If a branch signals completion (-1), STOP_DONE is
/// returned immediately.
pub fn compile_machine(ctx: &mut CompilerCtx, machine: &Machine<'_>, quantum: i64) -> Result<()> {
    let machine_ident = machine.ident.to_string();
    debug!("  branches: {}", machine.items.len());
    ctx.add_machine(&machine_ident);

    let curr_machine = ctx.get_current_machine();
    ctx.set_current_machine(Some(machine_ident.to_string()));

    let ptr_ty = ctx.module().target_config().pointer_type();
    let mut module_ctx = ctx.module().make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);
    ctx.begin_function();

    // Signature: (ctx_fat_ptr: ptr) -> (stop_code: ptr)
    builder.func.signature.params.push(AbiParam::new(ptr_ty));
    builder.func.signature.returns.push(AbiParam::new(ptr_ty));

    // ── Create all blocks up front ────────────────────────────────────────────
    let entry = builder.create_block();
    let machine_switch_block = builder.create_block();
    let default_block = builder.create_block();

    // CPS blocks jump here with (next_block_id) instead of returning.
    let quantum_continue_block = builder.create_block();
    // After "not done" check: store BLOCK_ID, decrement, re-dispatch or yield.
    let quantum_loop_block = builder.create_block();
    let quantum_stop_done_block = builder.create_block();
    let quantum_stop_limit_block = builder.create_block();

    builder.append_block_param(entry, ptr_ty);
    builder.append_block_param(quantum_continue_block, ptr_ty); // next_block_id
    builder.append_block_param(quantum_loop_block, ptr_ty); // next_block_id

    // ── Entry block ───────────────────────────────────────────────────────────
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let ctx_fat_ptr = builder.block_params(entry)[0];

    let machine_ctx_var = builder.declare_var(ptr_ty);
    let quantum_var = builder.declare_var(ptr_ty);

    builder.def_var(machine_ctx_var, ctx_fat_ptr);
    let quantum_init = builder.ins().iconst(ptr_ty, quantum);
    builder.def_var(quantum_var, quantum_init);

    builder.ins().jump(machine_switch_block, &[]);

    // ── Register quantum_continue_block so CPS compilers can jump to it ───────
    ctx.set_quantum_block(quantum_continue_block);

    // ── Compile branches ──────────────────────────────────────────────────────
    let mut machine_switch = Switch::new();
    for (branch_id, item) in machine.items.iter().enumerate() {
        let MachineItem::Branch(ref branch) = item.inner else {
            bail!("Except branch, but found: {:?}", item);
        };

        compile_branch(
            ctx,
            &mut builder,
            &machine_ident,
            &mut machine_switch,
            branch_id as u128,
            branch,
            machine_ctx_var,
        )?;
    }

    // ── machine_switch_block: dispatch on BRANCH_ID ───────────────────────────
    builder.switch_to_block(machine_switch_block);
    let ctx_fat_ptr = builder.use_var(machine_ctx_var);

    // INLINED: was rt_load_u64(ctx_fat_ptr, BRANCH_ID)
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_fat_ptr, 0);
    let branch_id = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        exec_start,
        ExecCtxLayout::BRANCH_ID,
    );
    machine_switch.emit(&mut builder, branch_id, default_block);

    // ── default_block: no matching branch → STOP_DONE ────────────────────────
    builder.switch_to_block(default_block);
    let v = builder.ins().iconst(ptr_ty, STOP_DONE);
    builder.ins().return_(&[v]);

    // ── quantum_continue_block: first check — is the branch done? ─────────────
    builder.switch_to_block(quantum_continue_block);
    let next_id = builder.block_params(quantum_continue_block)[0];
    let is_done = builder.ins().icmp_imm(IntCC::Equal, next_id, -1);
    builder.ins().brif(
        is_done,
        quantum_stop_done_block,
        &[],
        quantum_loop_block,
        &[BlockArg::Value(next_id)],
    );

    // ── quantum_loop_block: store BLOCK_ID, decrement, re-dispatch or yield ───
    builder.switch_to_block(quantum_loop_block);
    let next_id = builder.block_params(quantum_loop_block)[0];

    // Write next_block_id into exec_ctx.BLOCK_ID (machine reads it on next loop).
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    builder.ins().store(
        MemFlags::trusted(),
        next_id,
        exec_start,
        ExecCtxLayout::BLOCK_ID,
    );

    let remaining = builder.use_var(quantum_var);
    let new_remaining = builder.ins().iadd_imm(remaining, -1);
    builder.def_var(quantum_var, new_remaining);

    let is_exhausted = builder.ins().icmp_imm(IntCC::Equal, new_remaining, 0);
    builder.ins().brif(
        is_exhausted,
        quantum_stop_limit_block,
        &[],
        machine_switch_block,
        &[],
    );

    // ── quantum_stop_done_block: STOP_DONE ────────────────────────────────────
    builder.switch_to_block(quantum_stop_done_block);
    let v = builder.ins().iconst(ptr_ty, STOP_DONE);
    builder.ins().return_(&[v]);

    // ── quantum_stop_limit_block: STOP_LIMIT ──────────────────────────────────
    builder.switch_to_block(quantum_stop_limit_block);
    let v = builder.ins().iconst(ptr_ty, STOP_LIMIT);
    builder.ins().return_(&[v]);

    builder.seal_all_blocks();

    trace!("CLIF [{}]:\n{}", machine_ident, builder.func);

    // ── Emit the function ─────────────────────────────────────────────────────
    let machine_sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function(&machine_ident, Linkage::Export, &machine_sig)?;

    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    ctx.set_current_machine(curr_machine);

    Ok(())
}
