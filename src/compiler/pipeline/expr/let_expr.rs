use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Variable},
};
use lake_frontend::api::{ast::Type, expr::Expr};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, compile_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile `let ident: ty [= default]`.
///
/// If `default` is present, it is compiled first (which stores its result in
/// `TEMP_VAL`). Then we open a new block that reads `TEMP_VAL` and writes it
/// into the variables array at the slot assigned to `ident`.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    ident: &str,
    ty: &Type<'_>,
    default: Option<&Expr<'_>>,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    // Compile the initialiser (if any) first; it leaves its result in TEMP_VAL.
    let next_id = match default {
        Some(d) => match compile_expr(ctx, builder, machine_ctx_var, block_id, branch_switch, state, d)? {
            StmtOutcome::Continue(id) => id,
            // A terminal default is unusual but we propagate it.
            terminal => return Ok(terminal),
        },
        None => block_id,
    };

    let b = builder.create_block();
    builder.switch_to_block(b);

    // Register the variable and get its slot index.
    let cranelift_ty = ctx
        .lookup_type(&ty.to_string())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown type '{}'", ty.to_string()))?
        .unwrap_simple();
    let var_index = state.insert_with_lake_type(ident.to_string(), cranelift_ty, ty.to_string());

    // Read TEMP_VAL (the initialiser result) and write it into vars[var_index].
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
    let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);

    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let temp_val = builder
        .ins()
        .call(load_u64_ref, &[ctx_ptr, temp_val_offset]);
    let temp_val = builder.inst_results(temp_val)[0];

    let vars_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
    let vars_ptr_call = builder.ins().call(load_u64_ref, &[ctx_ptr, vars_offset]);
    let vars_ptr = builder.inst_results(vars_ptr_call)[0];

    let var_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[vars_ptr, temp_val, size, var_offset]);

    let next_block_id = builder.ins().iconst(ptr_ty, next_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_block_id)]);

    branch_switch.set_entry(next_id as u128, b);
    Ok(StmtOutcome::Continue(next_id + 1))
}
