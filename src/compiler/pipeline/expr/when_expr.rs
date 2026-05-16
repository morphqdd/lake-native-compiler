use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{DataDescription, Linkage, Module},
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
};
use lake_frontend::api::expr::Expr;
use log::debug;

use crate::compiler::{
    ctx::CompilerCtx,
    mphf::{self, MphfBuilder, emit_fxhash, emit_hash_function, emit_mphf_lookup, fxhash},
    pipeline::expr::StmtOutcome,
    rt::{alloc_static_buffer, layout::ExecCtxLayout},
};

use super::{BranchState, compile_expr};

enum WhenBranchType {
    Simple,
    Ptr,
}

/// True for the wildcard pattern `_` used as a default arm in `when`.
fn is_wildcard(expr: &Expr<'_>) -> bool {
    matches!(expr, Expr::Var("_", _))
}

pub fn compile<'a>(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    outer_switch: &mut Switch,
    state: &mut BranchState,
    cond_expr: &Expr<'a>,
    branches: Vec<(Expr<'a>, Vec<Expr<'a>>)>,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // Detect a wildcard `_` arm — at most one is allowed.  When present it
    // becomes the default destination of the dispatch switch instead of the
    // implicit "fall through to after_when" path.
    let wildcard_idx: Option<usize> = {
        let positions: Vec<usize> = branches
            .iter()
            .enumerate()
            .filter(|(_, (cond, _))| is_wildcard(cond))
            .map(|(i, _)| i)
            .collect();
        if positions.len() > 1 {
            bail!(
                "`when` accepts at most one wildcard `_` arm, got {} (at indices {:?})",
                positions.len(),
                positions
            );
        }
        positions.into_iter().next()
    };

    let b_check = builder.create_block();
    let b_ret: Vec<_> = (0..branches.len())
        .map(|_| builder.create_block())
        .collect();
    let b_no_match = builder.create_block();

    let disc_done_id = match compile_expr(
        ctx,
        builder,
        machine_ctx_var,
        block_id,
        outer_switch,
        state,
        cond_expr,
        None,
        None,
        false,
    )? {
        StmtOutcome::Continue(id) => id,
        other => bail!("`when` discriminant cannot be a terminal: {:?}", other),
    };

    let mut body_starts: Vec<i64> = Vec::with_capacity(branches.len());
    let mut redirect_info: Vec<(i64, cranelift::prelude::Block)> = Vec::new();
    let mut current_id = disc_done_id + 1;

    for (i, (_cond, body_exprs)) in branches.iter().enumerate() {
        body_starts.push(current_id);

        let mut branch_outcome = StmtOutcome::Continue(current_id);

        for expr_span in body_exprs {
            branch_outcome = compile_expr(
                ctx,
                builder,
                machine_ctx_var,
                branch_outcome.next_available(),
                outer_switch,
                state,
                expr_span,
                None,
                None,
                false,
            )?;
            if branch_outcome.is_terminal() {
                break;
            }
        }

        let next_available = branch_outcome.next_available();
        // Redirect from the arm's exit slot back to `after_when_id`.
        // Both `Continue` and `Wait` outcomes resume execution at
        // `next_available` — Continue immediately, Wait once the
        // reply lands — so both need a redirect that funnels them
        // back to the post-when code.  Only true terminal outcomes
        // (`StateChange`) skip the redirect.
        let needs_redirect = matches!(
            branch_outcome,
            StmtOutcome::Continue(_) | StmtOutcome::Wait { .. }
        );

        if i < branches.len() - 1 {
            if needs_redirect {
                let b_redirect = builder.create_block();
                redirect_info.push((next_available, b_redirect));
            }
            current_id = next_available + 1;
        } else {
            // Last arm: redirect to after_when_id is needed too,
            // otherwise its exit lands at the unregistered slot
            // after the arm's body and falls through to the
            // outer_switch default (STOP_DONE) — silently dropping
            // every statement that follows the `when` block.
            if needs_redirect {
                let b_redirect = builder.create_block();
                redirect_info.push((next_available, b_redirect));
                current_id = next_available + 1;
            } else {
                current_id = next_available;
            }
        }
    }

    let after_when_id = current_id;

    let qb = ctx.quantum_block();

    for (end_id, b_redirect) in &redirect_info {
        builder.switch_to_block(*b_redirect);
        let v = builder.ins().iconst(ptr_ty, after_when_id);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
        outer_switch.set_entry(*end_id as u128, *b_redirect);
    }

    for (i, &start_id) in body_starts.iter().enumerate() {
        builder.switch_to_block(b_ret[i]);
        let v = builder.ins().iconst(ptr_ty, start_id);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
    }

    builder.switch_to_block(b_no_match);
    {
        let v = builder.ins().iconst(ptr_ty, after_when_id);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
    }

    builder.switch_to_block(b_check);

    {
        let mut keys = vec![];
        let mut when_switch = Switch::new();
        // Pick the type from the first non-wildcard arm — wildcards have no
        // discriminant kind of their own.
        let typed_arm = branches
            .iter()
            .find(|(c, _)| !is_wildcard(c))
            .ok_or_else(|| {
                anyhow::anyhow!("`when` requires at least one typed arm in addition to `_`")
            })?;
        let when_branch_type = get_ty(&typed_arm.0)?;
        // Default destination of the dispatch switch: the wildcard arm's body
        // start when one exists, otherwise the implicit fall-through block.
        let default_block = match wildcard_idx {
            Some(idx) => b_ret[idx],
            None => b_no_match,
        };

        let ctx_ptr = builder.use_var(machine_ctx_var);
        let raw_ctx_ptr = builder.ins().load(ptr_ty, MemFlags::new(), ctx_ptr, 0);
        let temp_off = ExecCtxLayout::load(builder, ptr_ty, raw_ctx_ptr, ExecCtxLayout::TEMP_VAL);

        let index_ext = match when_branch_type {
            WhenBranchType::Simple => {
                for (i, (cond_span, _)) in branches.iter().enumerate() {
                    if is_wildcard(cond_span) {
                        continue;
                    }
                    when_switch.set_entry(literal_value(cond_span)?, b_ret[i]);
                }
                temp_off
            }
            WhenBranchType::Ptr => {
                // Track the original branch index for each non-wildcard key
                // so the switch can map the MPHF index back to the right
                // body block.
                let mut key_branch_idx: Vec<usize> = Vec::new();
                for (i, (cond_span, _)) in branches.iter().enumerate() {
                    if is_wildcard(cond_span) {
                        continue;
                    }
                    let key = hash_lit(cond_span)?;
                    keys.push(key);
                    key_branch_idx.push(i);
                }
                let mphf = MphfBuilder::build(&keys);

                let disp_data_id = ctx.module_mut().declare_data(
                    &format!("mphf_disp_{disc_done_id}"),
                    Linkage::Export,
                    false,
                    false,
                )?;
                let mut disp_data_desc = DataDescription::new();
                let mut disp_bytes = vec![];
                mphf.displacements.iter().for_each(|disp| {
                    disp.to_le_bytes()
                        .iter()
                        .for_each(|&byte| disp_bytes.push(byte))
                });
                debug!("MPHF: {mphf:?}");
                debug!(
                    "MPHF displacements: {:?} {:?}",
                    mphf.displacements, disp_bytes
                );
                disp_data_desc.define(disp_bytes.into());
                ctx.module_mut()
                    .define_data(disp_data_id, &disp_data_desc)?;

                let keys_data_id = ctx.module_mut().declare_data(
                    &format!("mphf_keys_{disc_done_id}"),
                    Linkage::Export,
                    false,
                    false,
                )?;
                let mut keys_data_desc = DataDescription::new();

                // The runtime verification (`keys_array[idx] == input_hash`)
                // requires the array to be laid out in **MPHF-permuted**
                // order: position `mphf.lookup(k)` must hold `k`.  Storing in
                // the original branch order only works when MPHF happens to
                // produce the identity mapping; with three or more keys
                // that's the exception, not the rule.
                let mut keys_table = vec![0u64; keys.len()];
                for (i, &key) in keys.iter().enumerate() {
                    let index = mphf.lookup(key) as usize;
                    keys_table[index] = key;
                    when_switch.set_entry(index as u128, b_ret[key_branch_idx[i]]);
                }

                let mut keys_bytes = vec![];
                for key in &keys_table {
                    keys_bytes.extend_from_slice(&key.to_le_bytes());
                }
                debug!("MPHF keys (permuted): {:?} {:?}", keys_table, keys_bytes);
                keys_data_desc.define(keys_bytes.into());
                ctx.module_mut()
                    .define_data(keys_data_id, &keys_data_desc)?;
                let start = builder.ins().load(ptr_ty, MemFlags::trusted(), temp_off, 0);
                let end = builder.ins().load(ptr_ty, MemFlags::trusted(), temp_off, 8);

                let len = builder.ins().isub(end, start);

                let fxhash_result = emit_fxhash(builder, start, len);

                let mphf_hash = emit_hash_function(builder, fxhash_result, mphf.seed);
                let disp_gv = ctx
                    .module_mut()
                    .declare_data_in_func(disp_data_id, builder.func);
                let disp_ptr = builder.ins().global_value(ptr_ty, disp_gv);
                let index = emit_mphf_lookup(builder, &mphf, mphf_hash, disp_ptr);

                let keys_gv = ctx
                    .module_mut()
                    .declare_data_in_func(keys_data_id, builder.func);
                let keys_ptr = builder.ins().global_value(ptr_ty, keys_gv);
                let index_ext = builder.ins().uextend(ptr_ty, index);
                let key_offset = builder.ins().imul_imm(index_ext, 8);
                let key_addr = builder.ins().iadd(keys_ptr, key_offset);
                let stored_key = builder.ins().load(ptr_ty, MemFlags::trusted(), key_addr, 0);
                let matches = builder.ins().icmp(IntCC::Equal, stored_key, fxhash_result);

                let verified_block = builder.create_block();
                builder
                    .ins()
                    .brif(matches, verified_block, &[], default_block, &[]);

                builder.switch_to_block(verified_block);
                builder.seal_block(verified_block);
                index_ext
            }
        };
        when_switch.emit(builder, index_ext, default_block);
    }
    outer_switch.set_entry(disc_done_id as u128, b_check);

    Ok(StmtOutcome::Continue(after_when_id))
}

fn literal_value(expr: &Expr<'_>) -> Result<u128> {
    match expr {
        Expr::Bool(false) => Ok(0),
        Expr::Bool(true) => Ok(1),
        Expr::Num(s, _) => Ok(lake_frontend::api::expr::parse_int_literal(s)? as u64 as u128),
        // An atom in a `when` arm dispatches on its compile-time hash —
        // identical to how the discriminant fold emits the value, so
        // equality holds.
        Expr::Atom(name) => Ok(super::pure_expr::atom_id(name) as u64 as u128),
        other => bail!("unsupported when condition: {:?}", other),
    }
}

fn hash_lit(expr: &Expr<'_>) -> Result<u64> {
    match expr {
        Expr::String(s, _) => Ok(fxhash(s.as_bytes())),
        other => bail!("unsupported when condition: {:?}", other),
    }
}

fn get_ty(expr: &Expr<'_>) -> Result<WhenBranchType> {
    match expr {
        Expr::Bool(false) => Ok(WhenBranchType::Simple),
        Expr::Bool(true) => Ok(WhenBranchType::Simple),
        Expr::Num(_, _) => Ok(WhenBranchType::Simple),
        Expr::Atom(_) => Ok(WhenBranchType::Simple),
        Expr::String(_, _) => Ok(WhenBranchType::Ptr),
        other => bail!("unsupported when condition: {:?}", other),
    }
}
