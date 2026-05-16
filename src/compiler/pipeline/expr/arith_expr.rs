use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
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
        ctx,
        builder,
        machine_ctx_var,
        block_id,
        branch_switch,
        state,
        lhs,
        None,
        None,
        false,
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
        // #81 — Inline all rt_load_u64 / rt_store calls.  These are
        // scheduler-trusted accesses (exec_ctx + vars buffer) — bounds
        // check skipped.  Profile showed rt_store at ~48% of SHA-256
        // runtime; eliminating its function-call overhead is the
        // single biggest opt available.
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        let lhs_val = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::TEMP_VAL,
        );
        let vars_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);
        builder.ins().store(
            MemFlags::trusted(),
            lhs_val,
            vars_start,
            tmp_slot as i32 * 8,
        );

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
        None,
        None,
        false,
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
        // #81 — inline all rt loads/stores (scheduler-trusted).
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        // Load variables fat ptr → start.
        let vars_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);
        // Load saved LHS from vars[tmp_slot].
        let lhs_val =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), vars_start, tmp_slot as i32 * 8);
        // Load RHS from TEMP_VAL.
        let rhs_val = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::TEMP_VAL,
        );

        // Compute result.
        let result = match op {
            BinaryOp::Add => builder.ins().iadd(lhs_val, rhs_val),
            BinaryOp::Sub => builder.ins().isub(lhs_val, rhs_val),
            BinaryOp::Mul => builder.ins().imul(lhs_val, rhs_val),
            BinaryOp::Div => builder.ins().sdiv(lhs_val, rhs_val),
            BinaryOp::Le => {
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedLessThanOrEqual, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
            BinaryOp::Ge => {
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, lhs_val, rhs_val);
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
                let cmp = builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThan, lhs_val, rhs_val);
                builder.ins().uextend(ptr_ty, cmp)
            }
        };

        // Store result to TEMP_VAL (inlined).
        builder.ins().store(
            MemFlags::trusted(),
            result,
            exec_start,
            ExecCtxLayout::TEMP_VAL,
        );

        let next = builder.ins().iconst(ptr_ty, rhs_done_id + 1);
        let qb = ctx.quantum_block();
        builder.ins().jump(qb, &[BlockArg::Value(next)]);
    }
    branch_switch.set_entry(rhs_done_id as u128, compute_block);

    Ok(StmtOutcome::Continue(rhs_done_id + 1))
}
