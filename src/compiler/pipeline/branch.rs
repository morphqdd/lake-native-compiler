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
    pipeline::expr::{BranchState, StmtOutcome, compile_expr, is_fast_chain_pair},
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
    // Use cached exec_start (compile_machine init'ed it) instead of re-loading.
    let exec_start = ctx.exec_start(builder, machine_ctx_var);
    let block_id = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::BLOCK_ID);
    builder
        .ins()
        .jump(branch_switch_block, &[BlockArg::Value(block_id)]);

    // ── Pre-allocate variable slots then compile body ─────────────────────
    let mut state = BranchState::default();
    let mut branch_switch = Switch::new();
    let mut block_id: i64 = 0;

    // Allocate one variable slot per pattern position, including guards and
    // wildcards.  The spawner / change_state always writes call args into
    // VARIABLES at slot[i] = arg[i], so the branch's var-name → slot mapping
    // must use the same position-based indexing — otherwise a branch with a
    // leading guard (`0 i64 a i64 b i64`) reads `a` from the guard slot.
    //
    // Wildcards and literal guards still consume a slot so subsequent named
    // params line up; they just don't get a binding name.
    for (pos, pattern) in patterns.iter().enumerate() {
        if pattern.is_wildcard() || pattern.is_literal_guard() {
            // Reserve the slot under an anonymous key so the index counter
            // stays in lock-step with pattern position.
            state.insert(format!("__pat_{pos}"), ptr_ty);
            continue;
        }
        let ident_str = Clean::<Ident<'_>>::clean(pattern).to_string();
        let lake_ty = Clean::<Type<'_>>::clean(pattern).to_string();
        state.insert_with_lake_type(ident_str, ptr_ty, lake_ty);
    }

    // #80 Level 2 — fast-path chaining.
    //
    // For each pair of adjacent statements where BOTH are fast-path
    // eligible (pure_expr or `let x = <pure>`), allocate a Cranelift
    // entry block for the successor and pass it as `fall_through` to
    // the current statement.  The fast handler emits
    // `dec quantum; brif zero, fast_yield[next], fall_through` in place
    // of `jump quantum_block(next)`, eliminating the machine_switch +
    // branch_switch indirect Switches on the hot path.
    //
    // Non-fast-path statements stay on the legacy qb route — they're
    // reached through branch_switch dispatch as normal.
    let mut current_entry: Option<cranelift::codegen::ir::Block> = None;
    for (i, expr) in branch.body.iter().enumerate() {
        let chain_to_next = branch
            .body
            .get(i + 1)
            .map(|next| is_fast_chain_pair(&expr.inner, &next.inner))
            .unwrap_or(false);

        let fall_through = if chain_to_next {
            Some(builder.create_block())
        } else {
            None
        };

        let outcome = compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            &mut branch_switch,
            &mut state,
            &expr,
            current_entry,
            fall_through,
        )?;

        match outcome {
            StmtOutcome::Continue(id) => block_id = id,
            other => {
                block_id = other.next_available();
                break;
            }
        }

        current_entry = fall_through;
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
