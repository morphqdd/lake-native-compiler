use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{ctx::CompilerCtx, pipeline::expr::StmtOutcome, rt::layout::ExecCtxLayout};

use super::{BranchState, compile_expr};

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
            )?;
            if branch_outcome.is_terminal() {
                break;
            }
        }

        let next_available = branch_outcome.next_available();

        if i < branches.len() - 1 {
            if !branch_outcome.is_terminal() {
                let b_redirect = builder.create_block();
                redirect_info.push((next_available, b_redirect));
            }
            current_id = next_available + 1;
        } else {
            current_id = next_available;
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
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let temp_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let call = builder.ins().call(load_ref, &[ctx_ptr, temp_off]);
        let discrim = builder.inst_results(call)[0];

        let mut when_switch = Switch::new();
        for (i, (cond_span, _)) in branches.iter().enumerate() {
            let key = literal_value(cond_span)?;
            when_switch.set_entry(key, b_ret[i]);
        }
        when_switch.emit(builder, discrim, b_no_match);
    }
    outer_switch.set_entry(disc_done_id as u128, b_check);

    Ok(StmtOutcome::Continue(after_when_id))
}

fn literal_value(expr: &Expr<'_>) -> Result<u128> {
    match expr {
        Expr::Bool(false) => Ok(0),
        Expr::Bool(true) => Ok(1),
        Expr::Num(s, _) => Ok(s.parse::<i64>()? as u64 as u128),
        other => bail!("unsupported when condition: {:?}", other),
    }
}
