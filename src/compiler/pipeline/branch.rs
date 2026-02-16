use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};
use lake_frontend::api::{
    ast::{Branch, Clean, Ident, Pattern, Type},
    expr::Expr,
};
use log::debug;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, compile_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile a single branch of a machine, appending blocks to the
/// already-open `builder` / `machine_switch`.
///
/// Returns the updated context and the number of local variables.
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
    let rt_funcs = ctx.rt_funcs().clone();
    let patterns = Clean::<Vec<Pattern<'_>>>::clean(branch);

    let (hash, param_count) = crate::compiler::hash_pattern(&patterns);

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
    let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
    let block_id_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::BLOCK_ID as i64);
    let call = builder
        .ins()
        .call(load_u64_ref, &[ctx_ptr, block_id_offset]);
    let block_id = builder.inst_results(call)[0];
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
            block_id = compile_expr(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                &mut branch_switch,
                &mut state,
                &Expr::from(pattern),
            )?;
        }
    }

    for expr in branch.body.iter() {
        block_id = compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            &mut branch_switch,
            &mut state,
            &expr,
        )?;
    }

    // ── Emit the per-branch block switch ──────────────────────────────────────
    builder.switch_to_block(branch_switch_block);
    let block_id_val = builder.block_params(branch_switch_block)[0];
    branch_switch.emit(builder, block_id_val, default_branch_block);

    // ── Register pattern metadata ─────────────────────────────────────────────
    debug!(
        "  branch[{}]: hash={:#018x}, params={}, vars={}, blocks={}",
        branch_id,
        hash,
        param_count,
        state.len(),
        block_id,
    );
    ctx.insert_pattern(machine_ident, hash, param_count, branch_id, state.len())?;

    Ok(())
}
