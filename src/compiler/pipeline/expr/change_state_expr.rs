use crate::compiler::pipeline::expr::{StmtOutcome, dispatch};
use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{ExecCtxLayout, FatPtrLayout},
};

pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    machine_name: &str,
    call_hash: u64,
    jump_args_base: usize,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();

    let candidates = ctx.branches_for_hash(machine_name, call_hash);
    anyhow::ensure!(
        !candidates.is_empty(),
        "No branch matching call hash {:#018x} in '{}'",
        call_hash,
        machine_name
    );

    let arg_count = candidates.iter().map(|c| c.param_count).max().unwrap_or(0);
    let needs_guard_dispatch = candidates.len() > 1;

    let b = builder.create_block();
    builder.switch_to_block(b);

    // #81 — Inline rt_load_u64 / rt_store at use sites. `self()` is called
    // on every loop iteration of self-recursive ret-machines (fill_w/compress
    // in SHA-256), so eliminating function-call overhead here matters.
    let spawning_ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), spawning_ctx_ptr, 0);

    let spawning_ja_start = if arg_count > 0 || needs_guard_dispatch {
        let ja_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::JUMP_ARGS,
        );
        Some(builder.ins().load(ptr_ty, MemFlags::trusted(), ja_fat, 0))
    } else {
        None
    };

    if arg_count > 0 {
        let vars_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);
        let ja_start = spawning_ja_start.unwrap();
        for i in 0..arg_count {
            let val = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                ja_start,
                (jump_args_base + i) as i32 * 8,
            );
            builder
                .ins()
                .store(MemFlags::trusted(), val, vars_start, i as i32 * 8);
        }
    }

    let branch_id_val = if needs_guard_dispatch {
        let disc_pos = dispatch::find_best_guard_pos(&candidates);
        let disc = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            spawning_ja_start.unwrap(),
            (jump_args_base + disc_pos) as i32 * 8,
        );
        let namespace = ctx.next_dispatch_id();
        dispatch::emit_guard_select(ctx, builder, ptr_ty, &candidates, disc, namespace)?
    } else {
        builder.ins().iconst(ptr_ty, candidates[0].branch_id as i64)
    };

    builder.ins().store(
        MemFlags::trusted(),
        branch_id_val,
        exec_start,
        ExecCtxLayout::BRANCH_ID,
    );
    let _ = rt_funcs;

    let next_id = 0;
    let next_id_val = builder.ins().iconst(ptr_ty, next_id);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id_val)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::StateChange {
        next_available: block_id + 1,
    })
}
