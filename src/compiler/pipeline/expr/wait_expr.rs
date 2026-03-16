use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
};
use lake_frontend::api::ast::{Branch, Clean, Ident, Pattern, Type};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, compile_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile a `wait { handler_pattern -> { body } }` expression.
///
/// ## Generated block structure
///
/// ```text
/// Block N (wait_check, registered in branch_switch):
///   load HEAD, TAIL from exec_ctx
///   HEAD != TAIL  →  dequeue_block
///   HEAD == TAIL  →  suspend_block
///
/// dequeue_block:
///   read mailbox[HEAD * 8] for each handler arg
///   store into VARIABLES[handler_offset..]
///   advance HEAD (mod 256)
///   jump quantum_continue(handler_body_start)
///
/// suspend_block:
///   store BLOCK_ID = N  (re-enter here when woken)
///   jump quantum_continue(-3)  →  STOP_WAIT
///
/// Block N+1... (handler body):
///   compiled handler body expressions
/// ```
pub(crate) fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder<'_>,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    collect: Vec<Branch<'_>>,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();
    let qb = ctx.quantum_block();

    // V1: single handler (first from collect)
    let handler = collect
        .first()
        .ok_or_else(|| anyhow::anyhow!("wait expression must have at least one handler"))?;

    let patterns = Clean::<Vec<Pattern<'_>>>::clean(handler);

    // Handler arg count = non-default patterns
    let handler_arg_count = patterns.iter().filter(|p| p.default.is_none()).count();

    // Handler variables start at the current state offset
    let handler_var_base = state.len();

    // Register handler pattern variables in state
    for pattern in &patterns {
        if pattern.default.is_none() {
            let ident_str = Clean::<Ident<'_>>::clean(pattern).to_string();
            let lake_ty = Clean::<Type<'_>>::clean(pattern).to_string();
            state.insert_with_lake_type(ident_str, ptr_ty, lake_ty);
        }
    }

    // Handler body starts at block_id + 1
    let handler_body_start = block_id + 1;

    // ── Internal Cranelift blocks (not in branch_switch) ────────────────
    let wait_check_block = builder.create_block();
    let dequeue_block = builder.create_block();
    let suspend_block = builder.create_block();

    // ── Block N: wait_check ─────────────────────────────────────────────
    builder.switch_to_block(wait_check_block);
    let ctx_ptr = builder.use_var(machine_ctx_var);

    // Load exec_ctx start pointer
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    let head = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_HEAD);
    let tail = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_TAIL);

    let has_msg = builder.ins().icmp(IntCC::NotEqual, head, tail);
    builder
        .ins()
        .brif(has_msg, dequeue_block, &[], suspend_block, &[]);

    // ── dequeue_block: read from mailbox, populate handler vars ─────────
    builder.switch_to_block(dequeue_block);
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);

    // Load mailbox fat ptr and HEAD
    let mailbox_fat = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_FAT);
    let head = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_HEAD);

    // Load VARIABLES fat ptr
    let vars_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let call_vars = builder.ins().call(load_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(call_vars)[0];

    // Dequeue: copy mailbox[HEAD..HEAD+N] → VARIABLES[handler_var_base..handler_var_base+N]
    let size8 = builder.ins().iconst(ptr_ty, 8);
    for i in 0..handler_arg_count {
        let msg_index = builder.ins().iadd_imm(head, i as i64);
        // mod 256
        let msg_index_mod = builder.ins().band_imm(msg_index, 255);
        let msg_offset = builder.ins().imul_imm(msg_index_mod, 8);
        let call_load_msg = builder.ins().call(load_ref, &[mailbox_fat, msg_offset]);
        let msg_val = builder.inst_results(call_load_msg)[0];

        let var_slot_offset = builder
            .ins()
            .iconst(ptr_ty, (handler_var_base + i) as i64 * 8);
        builder
            .ins()
            .call(store_ref, &[vars_ptr, msg_val, size8, var_slot_offset]);
    }

    // Advance HEAD: (HEAD + handler_arg_count) mod 256
    let new_head = builder
        .ins()
        .iadd_imm(head, handler_arg_count as i64);
    let new_head_mod = builder.ins().band_imm(new_head, 255);
    ExecCtxLayout::store(
        builder,
        new_head_mod,
        exec_start,
        ExecCtxLayout::MAILBOX_HEAD,
    );

    // Jump to handler body
    let handler_start_val = builder.ins().iconst(ptr_ty, handler_body_start);
    builder
        .ins()
        .jump(qb, &[BlockArg::Value(handler_start_val)]);

    // ── suspend_block: store resume BLOCK_ID, return STOP_WAIT ──────────
    builder.switch_to_block(suspend_block);
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    // Store BLOCK_ID = block_id (this block's ID) so re-entry returns here
    let block_id_val = builder.ins().iconst(ptr_ty, block_id);
    ExecCtxLayout::store(
        builder,
        block_id_val,
        exec_start,
        ExecCtxLayout::BLOCK_ID,
    );

    let wait_stop = builder.ins().iconst(ptr_ty, -3i64);
    builder.ins().jump(qb, &[BlockArg::Value(wait_stop)]);

    // Register wait_check_block in branch_switch at block_id
    branch_switch.set_entry(block_id as u128, wait_check_block);

    // ── Compile handler body expressions ────────────────────────────────
    let mut current_id = handler_body_start;
    for expr in &handler.body {
        match compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            current_id,
            branch_switch,
            state,
            &expr.inner,
        )? {
            StmtOutcome::Continue(id) => current_id = id,
            outcome => {
                current_id = outcome.next_available();
                break;
            }
        }
    }

    Ok(StmtOutcome::Wait {
        next_avaitlable: current_id,
    })
}
