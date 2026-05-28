use crate::compiler::pipeline::expr::{StmtOutcome, dispatch};
use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{FuncOrDataId, Module},
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{ExecCtxLayout, FatPtrLayout, process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout},
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
    arg_types: &[String],
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

    // #138 phase 2e — arena reset on `self(...)` to break the
    // long-running-actor leak.  Each iteration of a self-looping
    // actor used to bump the arena forever (or fall back to
    // rt_allocate_raw with no free), eating memory per iteration.
    //
    // Approach: before the new iteration starts, snapshot any
    // pointer-typed args into a temporary scratch buffer, reset
    // the arena bump cursor to base, then re-copy args into the
    // fresh arena via rt_copy_to_arena.  Scalar args (i64/atom/
    // pid/bool) pass through unchanged.
    //
    // Cost: 2 memcpy per pointer arg + 1 scratch alloc/free per
    // self() call.  Negligible vs the unbounded arena leak.
    if arg_count > 0 {
        let vars_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fat, 0);
        let ja_start = spawning_ja_start.unwrap();

        // Per-arg metadata for the snapshot/restore loop.
        struct ArgInfo {
            is_ptr: bool,
            val: cranelift::prelude::Value,
            size: Option<cranelift::prelude::Value>,
        }
        let mut args: Vec<ArgInfo> = Vec::with_capacity(arg_count);
        let mut total_ptr_size_opt: Option<cranelift::prelude::Value> = None;
        for i in 0..arg_count {
            let val = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                ja_start,
                (jump_args_base + i) as i32 * 8,
            );
            let is_ptr = arg_types
                .get(i)
                .map(|s| crate::compiler::is_pointer_like_type(s))
                .unwrap_or(false);
            let size = if is_ptr {
                // size = val.end - val.start
                let start = builder.ins().load(ptr_ty, MemFlags::trusted(), val, 0);
                let end = builder.ins().load(ptr_ty, MemFlags::trusted(), val, 8);
                let sz = builder.ins().isub(end, start);
                // Accumulate total.
                total_ptr_size_opt = Some(match total_ptr_size_opt {
                    None => sz,
                    Some(prev) => builder.ins().iadd(prev, sz),
                });
                Some(sz)
            } else {
                None
            };
            args.push(ArgInfo { is_ptr, val, size });
        }

        // If any pointer args, snapshot their bytes, reset arena,
        // then re-copy into the fresh arena.  If none, just reset
        // arena (cheap) and store scalars to vars.
        let proc_ctx_fat_ptr = {
            // Locate the current actor's proc_ctx via the scheduler.
            let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
                Some(FuncOrDataId::Data(id)) => id,
                _ => bail!("sheduler_ctx_fat_ptr global not found"),
            };
            let sched_gv = ctx
                .module_mut()
                .declare_data_in_func(sched_data_id, &mut builder.func);
            let sched_fat = builder.ins().global_value(ptr_ty, sched_gv);
            let sched_start = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sched_fat, 0);
            let cur_idx = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::CURRENT_PROCESS,
            );
            let proc_arr_fat = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::PROCESS_ARR_FAT,
            );
            let proc_arr = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), proc_arr_fat, 0);
            let cur_off = builder.ins().imul_imm(cur_idx, 8);
            let cur_entry_addr = builder.ins().iadd(proc_arr, cur_off);
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), cur_entry_addr, 0)
        };

        // Load arena fat-ptr and base — reset only if we own (BASE != 0).
        let proc_ctx_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), proc_ctx_fat_ptr, 0);
        let arena_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_FAT,
        );
        let arena_base = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_BASE,
        );

        // Same is_own_arena gate as compile_fused_self_call (#152
        // followup): snapshot+recopy is only needed when the actor
        // owns its arena (non-ret machine) — for sync ret-machines
        // arena is inherited and reset is a no-op; doing snapshot+
        // recopy still bumps the inherited arena per iter, which on
        // SHA-256's `compress` self-loop (3 buf args × 64 iter per
        // block) exhausted the caller's arena after ~14 compress
        // calls.  Measured 944× slowdown vs Rust pre-fix.
        let is_own_arena = !ctx.is_ret_machine(machine_name);
        let has_any_ptr_arg = is_own_arena && args.iter().any(|a| a.is_ptr);

        if has_any_ptr_arg {
            // Snapshot pointer args' bytes into a fresh scratch buf.
            let rt_alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => bail!("rt_allocate_raw not declared"),
            };
            let alloc_raw_ref = ctx
                .module_mut()
                .declare_func_in_func(rt_alloc_raw_id, &mut builder.func);
            let rt_copy_bytes_id = match ctx.module().get_name("rt_copy_bytes") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => bail!("rt_copy_bytes not declared"),
            };
            let copy_bytes_ref = ctx
                .module_mut()
                .declare_func_in_func(rt_copy_bytes_id, &mut builder.func);
            let rt_free_id = match ctx.module().get_name("rt_free") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => bail!("rt_free not declared"),
            };
            let free_ref = ctx
                .module_mut()
                .declare_func_in_func(rt_free_id, &mut builder.func);
            let rt_copy_to_arena_id = match ctx.module().get_name("rt_copy_to_arena") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => bail!("rt_copy_to_arena not declared"),
            };
            let copy_to_arena_ref = ctx
                .module_mut()
                .declare_func_in_func(rt_copy_to_arena_id, &mut builder.func);

            let total_size = total_ptr_size_opt.unwrap();
            let call_scratch = builder.ins().call(alloc_raw_ref, &[total_size]);
            let scratch_fat = builder.inst_results(call_scratch)[0];
            let scratch_start = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), scratch_fat, 0);

            // Pack pointer args' bytes into scratch sequentially.
            // Also record per-arg (offset, size) for the restore pass.
            let mut scratch_offsets: Vec<Option<(cranelift::prelude::Value, cranelift::prelude::Value)>> =
                Vec::with_capacity(arg_count);
            let mut cursor = builder.ins().iconst(ptr_ty, 0);
            let zero = builder.ins().iconst(ptr_ty, 0);
            for a in &args {
                if a.is_ptr {
                    let sz = a.size.unwrap();
                    // rt_copy_bytes(scratch_fat, cursor, a.val, 0, sz)
                    builder
                        .ins()
                        .call(copy_bytes_ref, &[scratch_fat, cursor, a.val, zero, sz]);
                    scratch_offsets.push(Some((cursor, sz)));
                    cursor = builder.ins().iadd(cursor, sz);
                } else {
                    scratch_offsets.push(None);
                }
            }

            // Reset arena bump cursor to base.
            let reset_block = builder.create_block();
            let after_reset_block = builder.create_block();
            let owns = builder.ins().icmp_imm(
                cranelift::prelude::IntCC::NotEqual,
                arena_base,
                0,
            );
            builder
                .ins()
                .brif(owns, reset_block, &[], after_reset_block, &[]);
            builder.switch_to_block(reset_block);
            builder.seal_block(reset_block);
            builder
                .ins()
                .store(MemFlags::trusted(), arena_base, arena_fat, 0);
            builder.ins().jump(after_reset_block, &[]);
            builder.switch_to_block(after_reset_block);
            builder.seal_block(after_reset_block);

            // Re-copy each pointer arg from scratch into the now-empty
            // arena, then store the new fat-ptr into vars.  For scalar
            // args, store the original val.
            for (i, a) in args.iter().enumerate() {
                let new_val = if let Some((off, sz)) = scratch_offsets[i] {
                    // Build a temporary fat-ptr that points at scratch[off..off+sz]
                    // so we can hand it to rt_copy_to_arena (which expects a
                    // fat-ptr address as src).  Use scratch_fat + off mutated:
                    // simpler — compute a transient fat-ptr struct on-the-fly via
                    // rt_copy_bytes directly into the arena.
                    //
                    // We can't easily build a transient fat-ptr inline; so use
                    // rt_arena_alloc(sz) to get a fresh dst, then rt_copy_bytes
                    // from scratch payload into dst payload.
                    let rt_arena_alloc_id = match ctx.module().get_name("rt_arena_alloc") {
                        Some(FuncOrDataId::Func(id)) => id,
                        _ => bail!("rt_arena_alloc not declared"),
                    };
                    let arena_alloc_ref = ctx
                        .module_mut()
                        .declare_func_in_func(rt_arena_alloc_id, &mut builder.func);
                    let call_dst = builder.ins().call(arena_alloc_ref, &[sz]);
                    let dst_fat = builder.inst_results(call_dst)[0];
                    // rt_copy_bytes(dst_fat, 0, scratch_fat, off, sz)
                    builder
                        .ins()
                        .call(copy_bytes_ref, &[dst_fat, zero, scratch_fat, off, sz]);
                    let _ = copy_to_arena_ref;
                    dst_fat
                } else {
                    a.val
                };
                builder
                    .ins()
                    .store(MemFlags::trusted(), new_val, vars_start, i as i32 * 8);
            }

            // Free scratch.
            builder.ins().call(free_ref, &[scratch_fat]);
        } else {
            // No pointer args — just reset arena and copy scalars.
            let owns = builder.ins().icmp_imm(
                cranelift::prelude::IntCC::NotEqual,
                arena_base,
                0,
            );
            let reset_block = builder.create_block();
            let after_reset_block = builder.create_block();
            builder
                .ins()
                .brif(owns, reset_block, &[], after_reset_block, &[]);
            builder.switch_to_block(reset_block);
            builder.seal_block(reset_block);
            builder
                .ins()
                .store(MemFlags::trusted(), arena_base, arena_fat, 0);
            builder.ins().jump(after_reset_block, &[]);
            builder.switch_to_block(after_reset_block);
            builder.seal_block(after_reset_block);

            for (i, a) in args.iter().enumerate() {
                builder
                    .ins()
                    .store(MemFlags::trusted(), a.val, vars_start, i as i32 * 8);
            }
        }
    } else {
        // No args — reset arena unconditionally (when owned).
        let proc_ctx_fat_ptr = {
            let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
                Some(FuncOrDataId::Data(id)) => id,
                _ => bail!("sheduler_ctx_fat_ptr global not found"),
            };
            let sched_gv = ctx
                .module_mut()
                .declare_data_in_func(sched_data_id, &mut builder.func);
            let sched_fat = builder.ins().global_value(ptr_ty, sched_gv);
            let sched_start = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sched_fat, 0);
            let cur_idx = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::CURRENT_PROCESS,
            );
            let proc_arr_fat = builder.ins().load(
                ptr_ty,
                MemFlags::trusted(),
                sched_start,
                ShedulerCtxLayout::PROCESS_ARR_FAT,
            );
            let proc_arr = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), proc_arr_fat, 0);
            let cur_off = builder.ins().imul_imm(cur_idx, 8);
            let cur_entry_addr = builder.ins().iadd(proc_arr, cur_off);
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), cur_entry_addr, 0)
        };
        let proc_ctx_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), proc_ctx_fat_ptr, 0);
        let arena_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_FAT,
        );
        let arena_base = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::OWNED_ARENA_BASE,
        );
        let owns = builder.ins().icmp_imm(
            cranelift::prelude::IntCC::NotEqual,
            arena_base,
            0,
        );
        let reset_block = builder.create_block();
        let after_reset_block = builder.create_block();
        builder
            .ins()
            .brif(owns, reset_block, &[], after_reset_block, &[]);
        builder.switch_to_block(reset_block);
        builder.seal_block(reset_block);
        builder
            .ins()
            .store(MemFlags::trusted(), arena_base, arena_fat, 0);
        builder.ins().jump(after_reset_block, &[]);
        builder.switch_to_block(after_reset_block);
        builder.seal_block(after_reset_block);
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
