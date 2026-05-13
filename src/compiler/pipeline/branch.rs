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
    pipeline::expr::{BranchState, StmtOutcome, accepts_entry_pub, compile_expr},
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

    // Tail-self loop optimization: expose this branch's switch block +
    // id so change_state_expr can short-circuit `self(...)` calls into
    // a single-indirect-jump loop back-edge, skipping qb + machine_switch
    // dispatch.
    ctx.set_current_branch(branch_id, branch_entry_block, branch_switch_block);

    // ── Pre-allocate variable slots ───────────────────────────────────────
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
    let mut named_slots: Vec<usize> = Vec::new();
    for (pos, pattern) in patterns.iter().enumerate() {
        if pattern.is_wildcard() || pattern.is_literal_guard() {
            // Reserve the slot under an anonymous key so the index counter
            // stays in lock-step with pattern position.
            state.insert(format!("__pat_{pos}"), ptr_ty);
            continue;
        }
        let ident_str = Clean::<Ident<'_>>::clean(pattern).to_string();
        let lake_ty = Clean::<Type<'_>>::clean(pattern).to_string();
        let slot = state.insert_with_lake_type(ident_str, ptr_ty, lake_ty);
        named_slots.push(slot);
    }

    // ── Branch entry: read BLOCK_ID and jump to the block switch ─────────────
    //
    // (Variable caching was attempted but caused phi-resolution overhead
    // that wiped out the memory-load savings — net +9M instructions on
    // CPU bench worker.  Cranelift's regalloc handles repeated mem loads
    // from trusted memory well enough on its own.)
    builder.switch_to_block(branch_entry_block);
    let exec_start = ctx.exec_start(builder, machine_ctx_var);
    let _ = named_slots;
    let block_id_load = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::BLOCK_ID);
    builder
        .ins()
        .jump(branch_switch_block, &[BlockArg::Value(block_id_load)]);

    // #80 Level 3 — super-block merging.
    //
    // Consecutive statements that all accept_entry (pure_expr or
    // `let x = <pure>`) are merged into ONE Cranelift basic block.
    // Inside the super-block:
    //   - No per-statement create_block / brif terminator;
    //   - All vars_start / exec_start loads shared via Cranelift GVN;
    //   - Cranelift instruction scheduling + regalloc operate over the
    //     full sequence as one unit.
    //
    // The super-block ends with a single `emit_continue` (inline
    // quantum check + brif to next stmt's qb path).  Re-entry semantics
    // are preserved by registering set_entry(stmt_id, super_b) for
    // every statement in the run — re-entering at any of those ids
    // re-runs the whole super-block from `b`, idempotent because all
    // super-block-eligible statements are pure.
    //
    // Statements that don't accept entry (when, wait, jump, spawn,
    // self, non-pure let) end the super-block and emit normally.
    let mut super_b: Option<cranelift::codegen::ir::Block> = None;
    for (i, expr) in branch.body.iter().enumerate() {
        let this_fast = accepts_entry_pub(&expr.inner);
        let next_fast = branch
            .body
            .get(i + 1)
            .map(|e| accepts_entry_pub(&e.inner))
            .unwrap_or(false);

        let entry = if this_fast {
            if let Some(b) = super_b {
                Some(b)
            } else if next_fast {
                let b = builder.create_block();
                super_b = Some(b);
                Some(b)
            } else {
                None
            }
        } else {
            None
        };

        let omit_exit = this_fast && next_fast;

        let outcome = compile_expr(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            &mut branch_switch,
            &mut state,
            &expr,
            entry,
            None,
            omit_exit,
        )?;

        match outcome {
            StmtOutcome::Continue(id) => block_id = id,
            other => {
                block_id = other.next_available();
                break;
            }
        }

        if !omit_exit {
            super_b = None;
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
