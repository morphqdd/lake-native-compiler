use cranelift::{codegen::ir::BlockArg, module::Module, prelude::InstBuilder};
use lake_frontend::api::ast::Branch;

use crate::compiler::pipeline::expr::StmtOutcome;

pub(crate) fn compile(
    ctx: &mut crate::compiler::ctx::CompilerCtx,
    builder: &mut cranelift::prelude::FunctionBuilder<'_>,
    machine_ctx_var: cranelift::prelude::Variable,
    block_id: i64,
    branch_switch: &mut cranelift::frontend::Switch,
    state: &mut super::BranchState,
    collect: Vec<Branch<'_>>,
) -> Result<super::StmtOutcome, anyhow::Error> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let b = builder.create_block();
    builder.switch_to_block(b);

    let wait_stop = builder.ins().iconst(ptr_ty, -3);

    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(wait_stop)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Wait {
        next_avaitlable: block_id + 1,
    })
}
