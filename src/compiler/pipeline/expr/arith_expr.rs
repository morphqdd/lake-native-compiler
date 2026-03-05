use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{ctx::CompilerCtx, pipeline::expr::StmtOutcome, rt::layout::ExecCtxLayout};

use super::{BranchState, compile_expr};

/// Supported binary operators.
#[derive(Clone, Copy)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Le,
    Ge,
    Eq,
    Lt,
    Gt,
}

/// Compile a binary operation `lhs OP rhs` in the CPS block model.
///
/// Produces four CPS blocks:
///   block_id     : compile LHS → TEMP_VAL, return block_id+1
///   block_id+1   : load TEMP_VAL → vars[tmp_slot], return block_id+2
///   block_id+2   : compile RHS → TEMP_VAL, return block_id+3  (may use more)
///   rhs_done_id  : load vars[tmp_slot] + TEMP_VAL, compute, → TEMP_VAL, return +1
///
/// Comparison ops produce 0 (false) or 1 (true) as an i64.
pub fn compile<'a>(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    lhs: &Expr<'a>,
    rhs: &Expr<'a>,
    op: BinaryOp,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // ── Step 1: Compile LHS → TEMP_VAL ───────────────────────────────────────
    let lhs_done_id = match compile_expr(
        ctx, builder, machine_ctx_var, block_id, branch_switch, state, lhs,
    )? {
        StmtOutcome::Continue(id) => id,
        other => bail!(
            "arithmetic LHS must be a simple expression, got terminal: {:?}",
            other
        ),
    };

    // ── Step 2: Save TEMP_VAL → vars[tmp_slot] ───────────────────────────────
    // Allocate a fresh variable slot for this operation (compile-time).
    let tmp_name = format!("__arith_tmp_{}", block_id);
    let tmp_slot = state.insert(tmp_name, ptr_ty);

    let save_block = builder.create_block();
    builder.switch_to_block(save_block);
    {
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ctx_ptr = builder.use_var(machine_ctx_var);

        // Load TEMP_VAL (LHS result).
        let temp_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let call = builder.ins().call(load_ref, &[ctx_ptr, temp_off]);
        let lhs_val = builder.inst_results(call)[0];

        // Load the variables fat ptr.
        let vars_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
        let vars_call = builder.ins().call(load_ref, &[ctx_ptr, vars_off]);
        let vars_fat_ptr = builder.inst_results(vars_call)[0];

        // Store LHS into vars[tmp_slot] via rt_store (writes to vars.start + slot*8).
        let tmp_slot_off = builder.ins().iconst(ptr_ty, tmp_slot as i64 * 8);
        let size = builder.ins().iconst(ptr_ty, 8);
        builder.ins().call(store_ref, &[vars_fat_ptr, lhs_val, size, tmp_slot_off]);

        let next = builder.ins().iconst(ptr_ty, lhs_done_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next)]);
    }
    branch_switch.set_entry(lhs_done_id as u128, save_block);

    // ── Step 3: Compile RHS → TEMP_VAL ───────────────────────────────────────
    let rhs_done_id = match compile_expr(
        ctx,
        builder,
        machine_ctx_var,
        lhs_done_id + 1,
        branch_switch,
        state,
        rhs,
    )? {
        StmtOutcome::Continue(id) => id,
        other => bail!(
            "arithmetic RHS must be a simple expression, got terminal: {:?}",
            other
        ),
    };

    // ── Step 4: Load vars[tmp_slot] + TEMP_VAL, compute, store → TEMP_VAL ────
    let compute_block = builder.create_block();
    builder.switch_to_block(compute_block);
    {
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ctx_ptr = builder.use_var(machine_ctx_var);

        // Load variables fat ptr.
        let vars_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
        let vars_call = builder.ins().call(load_ref, &[ctx_ptr, vars_off]);
        let vars_fat_ptr = builder.inst_results(vars_call)[0];

        // Load saved LHS from vars[tmp_slot].
        let tmp_slot_off = builder.ins().iconst(ptr_ty, tmp_slot as i64 * 8);
        let lhs_load = builder.ins().call(load_ref, &[vars_fat_ptr, tmp_slot_off]);
        let lhs_val = builder.inst_results(lhs_load)[0];

        // Load RHS from TEMP_VAL.
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let temp_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let rhs_call = builder.ins().call(load_ref, &[ctx_ptr, temp_off]);
        let rhs_val = builder.inst_results(rhs_call)[0];

        // Compute result.
        let result = match op {
            BinaryOp::Add => builder.ins().iadd(lhs_val, rhs_val),
            BinaryOp::Sub => builder.ins().isub(lhs_val, rhs_val),
            BinaryOp::Mul => builder.ins().imul(lhs_val, rhs_val),
            BinaryOp::Div => builder.ins().sdiv(lhs_val, rhs_val),
            BinaryOp::Le => {
                let cmp = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
            BinaryOp::Ge => {
                let cmp = builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
            BinaryOp::Eq => {
                let cmp = builder.ins().icmp(IntCC::Equal, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
            BinaryOp::Lt => {
                let cmp = builder.ins().icmp(IntCC::SignedLessThan, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
            BinaryOp::Gt => {
                let cmp = builder.ins().icmp(IntCC::SignedGreaterThan, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
        };

        // Store result to TEMP_VAL.
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let temp_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
        let size = builder.ins().iconst(ptr_ty, 8);
        builder.ins().call(store_ref, &[ctx_ptr, result, size, temp_off]);

        let next = builder.ins().iconst(ptr_ty, rhs_done_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next)]);
    }
    branch_switch.set_entry(rhs_done_id as u128, compute_block);

    Ok(StmtOutcome::Continue(rhs_done_id + 1))
}
