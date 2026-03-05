use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};
use lake_frontend::api::{
    ast::{Branch, Clean, Ident, Pattern, Type},
    expr::Expr,
};
use log::debug;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, compile_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile a single branch of a machine, appending blocks to the
/// already-open `builder` / `machine_switch`.
///
/// The pattern hash and param count are fetched from the registry (set during
/// the index pre-pass) rather than recomputed here.
pub fn compile_branch(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ident: &str,
    machine_switch: &mut Switch,
    branch_id: u128,
    branch: &Branch<'_>,
    machine_ctx_var: Variable,
) -> Result<()> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let patterns = Clean::<Vec<Pattern<'_>>>::clean(branch);

    // Fetch the hash that was computed once in the index pre-pass.
    let hash = ctx
        .get_branch_hash(machine_ident, branch_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Branch {branch_id} of '{machine_ident}' was not indexed — \
                 run the index pre-pass before compilation"
            )
        })?;

    let branch_entry_block = builder.create_block();
    let branch_switch_block = builder.create_block();
    builder.append_block_param(branch_switch_block, ptr_ty);

    let default_branch_block = builder.create_block();
    builder.switch_to_block(default_branch_block);
    let neg = builder.ins().iconst(ptr_ty, -1);
    builder.ins().return_(&[neg]);

    machine_switch.set_entry(branch_id, branch_entry_block);

    // ── Branch entry: read BLOCK_ID and jump to the block switch ─────────────
    builder.switch_to_block(branch_entry_block);
    let ctx_ptr = builder.use_var(machine_ctx_var);
    // INLINED: was rt_load_u64(ctx_ptr, BLOCK_ID)
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let block_id = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::BLOCK_ID);
    builder
        .ins()
        .jump(branch_switch_block, &[BlockArg::Value(block_id)]);

    // ── Compile pattern defaults then body expressions ─────────────────────
    let mut state = BranchState::default();
    let mut branch_switch = Switch::new();
    let mut block_id: i64 = 0;

    // Pre-allocate slots for non-default params so they occupy indices 0..N-1.
    // These are the "input parameters" of the branch, filled by the spawner.
    for pattern in &patterns {
        if pattern.default.is_none() {
            let ident_str = Clean::<Ident<'_>>::clean(pattern).to_string();
            let lake_ty = Clean::<Type<'_>>::clean(pattern).to_string();
            state.insert_with_lake_type(ident_str, ptr_ty, lake_ty);
        }
    }

    for pattern in &patterns {
        if pattern.default.is_some() {
            match compile_expr(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                &mut branch_switch,
                &mut state,
                &Expr::from(pattern),
            )? {
                StmtOutcome::Continue(id) => block_id = id,
                outcome => {
                    block_id = outcome.next_available();
                    break;
                }
            }
        }
    }

    for expr in branch.body.iter() {
        match compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            &mut branch_switch,
            &mut state,
            &expr,
        )? {
            StmtOutcome::Continue(id) => block_id = id,
            outcome => {
                block_id = outcome.next_available();
                break;
            }
        }
    }

    // ── Emit the per-branch block switch ──────────────────────────────────────
    builder.switch_to_block(branch_switch_block);
    let block_id_val = builder.block_params(branch_switch_block)[0];
    branch_switch.emit(builder, block_id_val, default_branch_block);

    // ── Update exact var_count in registry ────────────────────────────────────
    debug!(
        "  branch[{}]: hash={:#018x}, vars={}, blocks={}",
        branch_id,
        hash,
        state.len(),
        block_id,
    );
    ctx.update_branch_var_count(machine_ident, branch_id, state.len());

    Ok(())
}
