use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
        Type,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{
        ExecCtxLayout, FatPtrLayout, process_ctx::ProcessCtxLayout,
        sheduler_ctx::ShedulerCtxLayout,
    },
};

/// Build `rt_allocate(size: i64) -> i64`.
///
/// Returns a **fat-pointer address** to the start of a header `{start, end}`
/// where `[start..end)` is the usable payload region of length ≥ `size`.
///
/// Three-tier strategy:
///   * **Small (≤ 16 MiB):** size-class buckets (powers of two from 16 to
///     16 MiB, 21 buckets).  Hot path is `freelist_pop` → O(1) recycling.
///     If empty, bump-allocate from the heap.
///   * **Huge (> 16 MiB):** call `rt_mmap` directly for the payload, then
///     allocate a 16 B fat-pointer header from bucket 0.  Returned to the
///     kernel via `rt_munmap` on free — cannot leak.
pub fn define_allocate(ctx: CompilerCtx) -> Result<CompilerCtx> {
    define_allocate_impl(ctx, "rt_allocate", true)
}

/// Build `rt_allocate_raw(size: i64) -> i64`.
///
/// Identical to `rt_allocate` except the free-list pop path does NOT
/// zero the recycled payload.  Used by scheduler internals that
/// allocate buffers (exec_ctx, jump_args, mailbox, process_ctx, …)
/// whose every byte is overwritten by initialization code before any
/// read — making the zero-init wasted bandwidth.
///
/// User-facing `rt_allocate` keeps the zero-init guarantee that stdlib
/// helpers like `build_padded` rely on; only the scheduler's
/// well-known-size internal allocations route through `_raw`.
pub fn define_allocate_raw(ctx: CompilerCtx) -> Result<CompilerCtx> {
    define_allocate_impl(ctx, "rt_allocate_raw", false)
}

fn define_allocate_impl(
    mut ctx: CompilerCtx,
    func_name: &str,
    zero_on_pop: bool,
) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let (heap_curr_id, heap_end_id, free_list_id) = match (
        ctx.module().get_name("heap_curr"),
        ctx.module().get_name("heap_end"),
        ctx.module().get_name("free_list_heads"),
    ) {
        (
            Some(FuncOrDataId::Data(c)),
            Some(FuncOrDataId::Data(e)),
            Some(FuncOrDataId::Data(f)),
        ) => (c, e, f),
        _ => return Err(anyhow!(
            "Heap globals + free_list_heads must be declared before rt_allocate"
        )),
    };

    let mmap_id = match ctx.module().get_name("rt_mmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_mmap must be declared before rt_allocate")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let user_size = builder.block_params(entry)[0];

    let heap_curr_gv = ctx
        .module_mut()
        .declare_data_in_func(heap_curr_id, &mut builder.func);
    let heap_end_gv = ctx
        .module_mut()
        .declare_data_in_func(heap_end_id, &mut builder.func);
    let free_list_gv = ctx
        .module_mut()
        .declare_data_in_func(free_list_id, &mut builder.func);

    let heap_curr_ptr = builder.ins().global_value(ty, heap_curr_gv);
    let heap_end_ptr = builder.ins().global_value(ty, heap_end_gv);
    let free_list_ptr = builder.ins().global_value(ty, free_list_gv);

    let mmap_ref = ctx
        .module_mut()
        .declare_func_in_func(mmap_id, &mut builder.func);

    // ── Compute bucket index = ceil(log2(max(size, 16))) - 4 ────────────────
    // Sizes 16 / 32 / … / 16 MiB → buckets 0..20.
    // For huge (> 16 MiB) → bucket > 20 → take the direct-mmap path.
    let sixteen = builder.ins().iconst(ty, 16);
    let lt_min = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, user_size, sixteen);
    let size_clamped = builder.ins().select(lt_min, sixteen, user_size);
    let size_minus_one = builder.ins().iadd_imm(size_clamped, -1);
    // log2_ceil(x) = 64 - clz(x - 1) for x ≥ 1
    let lz = builder.ins().clz(size_minus_one);
    let total_bits = builder.ins().iconst(ty, 64);
    let log2_ceil = builder.ins().isub(total_bits, lz);
    let bucket_idx = builder.ins().iadd_imm(log2_ceil, -4);
    // bucket_size = 1 << (bucket_idx + 4)
    let four = builder.ins().iconst(ty, 4);
    let bucket_log = builder.ins().iadd(bucket_idx, four);
    let one = builder.ins().iconst(ty, 1);
    let bucket_size = builder.ins().ishl(one, bucket_log);

    // in_range = bucket_idx <= 20  (i.e. bucket_size <= 16 MiB)
    let max_bucket = builder.ins().iconst(ty, 20);
    let in_range = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bucket_idx, max_bucket);

    // ── Block layout ─────────────────────────────────────────────────────────
    // try_pop_block:    head = free_list[bucket]; if head != 0 jump pop_block
    // pop_block:        unlink head, jump merge_block(head)
    // bump_block:       bump-allocate bucket_size from heap (in-range only)
    // huge_block:       direct mmap path for size > MAX_BUCKET_SIZE
    // merge_block(fp):  return fp
    let try_pop_block = builder.create_block();
    let pop_block = builder.create_block();
    let bump_block = builder.create_block();
    let huge_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, ty);

    // Decide in-range (try freelist → bump) vs huge (direct mmap) up front.
    builder
        .ins()
        .brif(in_range, try_pop_block, &[], huge_block, &[]);

    // ── try_pop_block ────────────────────────────────────────────────────────
    builder.switch_to_block(try_pop_block);
    builder.seal_block(try_pop_block);
    let bucket_byte_offset = builder.ins().imul_imm(bucket_idx, 8);
    let head_addr = builder.ins().iadd(free_list_ptr, bucket_byte_offset);
    let head = builder.ins().load(ty, MemFlags::trusted(), head_addr, 0);
    let has_free = builder.ins().icmp_imm(IntCC::NotEqual, head, 0);
    builder
        .ins()
        .brif(has_free, pop_block, &[], bump_block, &[]);

    // ── pop_block: unlink head from free list ───────────────────────────────
    builder.switch_to_block(pop_block);
    builder.seal_block(pop_block);
    // payload_addr = *head    (head holds fat_ptr; fat_ptr.start = payload)
    let payload_addr = builder.ins().load(ty, MemFlags::trusted(), head, 0);
    // next = *payload_addr    (chain pointer stored at offset 0 of payload)
    let next = builder
        .ins()
        .load(ty, MemFlags::trusted(), payload_addr, 0);
    // free_list[bucket] = next
    builder
        .ins()
        .store(MemFlags::trusted(), next, head_addr, 0);

    if zero_on_pop {
        // Zero the recycled payload so callers see fresh memory (matches the
        // bump-from-mmap path's implicit zero-init).  We only need to clear the
        // bytes the caller actually requested (`user_size`, rounded up to the
        // 8-byte store stride) — anything beyond is outside their requested
        // range and they have no business reading it.  Previously we zeroed
        // the full `bucket_size`, which on spawn-heavy workloads (where the
        // free-list is hot and each actor recycles ~5 KB of bookkeeping) cost
        // ~30 ms / 100k iterations of pure memory bandwidth.
        let user_size_aligned = builder.ins().iadd_imm(user_size, 7);
        let zero_limit = builder.ins().band_imm(user_size_aligned, -8);

        let zero_hdr = builder.create_block();
        let zero_body = builder.create_block();
        builder.append_block_param(zero_hdr, ty);
        let zero_start = builder.ins().iconst(ty, 0);
        builder
            .ins()
            .jump(zero_hdr, &[BlockArg::Value(zero_start)]);

        builder.switch_to_block(zero_hdr);
        let zi = builder.block_params(zero_hdr)[0];
        let zcont = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, zi, zero_limit);
        builder
            .ins()
            .brif(zcont, zero_body, &[], merge_block, &[BlockArg::Value(head)]);

        builder.switch_to_block(zero_body);
        builder.seal_block(zero_body);
        let zaddr = builder.ins().iadd(payload_addr, zi);
        let zero_w = builder.ins().iconst(ty, 0);
        builder.ins().store(MemFlags::trusted(), zero_w, zaddr, 0);
        let zi_next = builder.ins().iadd_imm(zi, 8);
        builder.ins().jump(zero_hdr, &[BlockArg::Value(zi_next)]);
        builder.seal_block(zero_hdr);
    } else {
        // `_raw` variant: callers overwrite every byte before read, so the
        // zero loop is dead bandwidth.  Skip straight to merge.  Note that
        // we DO still need to clear at least the chain pointer (offset 0)
        // because the caller may read it as "zero" — but the scheduler's
        // internal callers all write their layout's first word
        // immediately (BRANCH_ID, func_ptr, …), so even that's redundant.
        // Defensive: store one zero at offset 0 to wipe the stale chain
        // pointer.  Single-cycle, no loop.
        let zero_w = builder.ins().iconst(ty, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), zero_w, payload_addr, 0);
        builder
            .ins()
            .jump(merge_block, &[BlockArg::Value(head)]);
    }
    let _ = user_size;

    // ── bump_block: classic bump allocation (in-range only) ─────────────────
    builder.switch_to_block(bump_block);
    builder.seal_block(bump_block);

    // Always bucket_size — huge allocations took the mmap path before this.
    let alloc_size = bucket_size;

    let heap_curr_addr = builder
        .ins()
        .load(ty, MemFlags::trusted(), heap_curr_ptr, 0);
    let heap_end_addr = builder
        .ins()
        .load(ty, MemFlags::trusted(), heap_end_ptr, 0);

    // Skip the 16-byte fat-pointer header to get the start of user data.
    let header = builder.ins().iconst(ty, FatPtrLayout::SIZE as i64);
    let raw_user_ptr = builder.ins().iadd(heap_curr_addr, header);

    // Align to 16 bytes.
    let align_mask = builder.ins().iconst(ty, !(16i64 - 1));
    let align_add = builder.ins().iconst(ty, 16 - 1);
    let unaligned = builder.ins().iadd(raw_user_ptr, align_add);
    let aligned_user_ptr = builder.ins().band(unaligned, align_mask);

    let end_addr = builder.ins().iadd(aligned_user_ptr, alloc_size);

    // Bounds check — heap exhausted is recoverable: instead of trapping
    // the whole program we mark the current actor as :dying so the next
    // quantum tick (machine.rs::quantum_loop_block) returns STOP_DONE and
    // the scheduler unlinks just that actor.  We still need to return a
    // value from this function — the caller expects a fat-ptr.  We hand
    // back a null fat-ptr (0); the dying actor has at most one CPS block
    // worth of execution before quantum_loop_block kills it.  If that
    // block touches the null pointer the program dies — accepted trade-
    // off for the MVP; a proper safe sentinel buffer is #87 follow-up.
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, end_addr, heap_end_addr);
    let oom_block = builder.create_block();
    let cont_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, cont_block, &[], oom_block, &[]);

    // ── oom_block: try to mark the current actor :dying.  Falls back to
    //    process-exit when there's no actor yet — true for the very early
    //    allocations during scheduler bootstrap (ShedulerCtxLayout::init,
    //    init_main_process) that run before `sheduler_ctx_fat_ptr` is
    //    populated or before any process is registered.
    builder.switch_to_block(oom_block);
    builder.seal_block(oom_block);

    // Resolve the scheduler context's address.  When the global value
    // (= the fat-ptr stored in the static) is still zero we're inside
    // very early init; take the process-exit branch.
    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found for OOM die path")),
    };
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    // The global slot holds a 16-byte fat-ptr `{ start, end }`; `start`
    // is the address of the sched_ctx struct itself, which is the
    // `sched_ptr` we need.  No extra deref — the first 8 bytes already
    // carry the struct address (or 0 when the slot hasn't been written
    // yet, which is what we test against here).
    let sched_fat_addr = builder.ins().global_value(ty, sched_gv);
    let sched_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), sched_fat_addr, 0);
    let check_count_block = builder.create_block();
    let init_exit_block = builder.create_block();
    let mark_actor_block = builder.create_block();
    let sched_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, sched_ptr, 0);
    builder
        .ins()
        .brif(sched_nonzero, check_count_block, &[], init_exit_block, &[]);

    // ── check_count_block: scheduler ctx exists; do we have at least
    //    one actor registered?  If not the failing allocation is part of
    //    init_main_process — fall through to process-exit.
    builder.switch_to_block(check_count_block);
    builder.seal_block(check_count_block);
    let real_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );
    let count_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, real_count, 0);
    builder
        .ins()
        .brif(count_nonzero, mark_actor_block, &[], init_exit_block, &[]);

    // ── mark_actor_block: standard actor-time path.  Find the running
    //    actor's exec_ctx, set IS_DYING, optionally log, return null.
    builder.switch_to_block(mark_actor_block);
    builder.seal_block(mark_actor_block);
    let proc_arr_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), proc_arr_fat, 0);
    let current_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let idx_scaled = builder.ins().imul_imm(current_idx, 8);
    let slot_addr = builder.ins().iadd(proc_arr_start, idx_scaled);
    let proc_ctx_fat = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 0);
    let proc_ctx_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), proc_ctx_fat, 0);
    let exec_ctx_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        proc_ctx_ptr,
        ProcessCtxLayout::EXEC_CTX,
    );
    let exec_ctx_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), exec_ctx_fat, 0);
    let one_dying = builder.ins().iconst(ty, 1);
    builder.ins().store(
        MemFlags::trusted(),
        one_dying,
        exec_ctx_ptr,
        ExecCtxLayout::IS_DYING,
    );

    // Optional crash log to stderr — gated at lakec invocation by
    // LAKE_DEATH_LOG=1.  Reading the env at compile time keeps the
    // produced binary free of any extra branches when disabled.
    let want_log = std::env::var("LAKE_DEATH_LOG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let syscall_ref_for_log = if want_log {
        let syscall_id = match ctx.module().get_name("rt_syscall") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => {
                return Err(anyhow!(
                    "rt_syscall must be declared before LAKE_DEATH_LOG-instrumented rt_allocate"
                ));
            }
        };
        Some(
            ctx.module_mut()
                .declare_func_in_func(syscall_id, &mut builder.func),
        )
    } else {
        None
    };

    // Helper closure: declare a unique `Local` data symbol holding `msg`
    // bytes and emit a write(2, …) syscall via the captured syscall ref.
    // Symbol name is suffixed by `func_name` + `tag` to keep
    // rt_allocate / rt_allocate_raw / actor / init messages distinct
    // (declare_data rejects duplicates within a module).
    let mut emit_log = |builder: &mut FunctionBuilder,
                        ctx: &mut CompilerCtx,
                        tag: &str,
                        msg: &str|
     -> Result<()> {
        let Some(syscall_ref) = syscall_ref_for_log else {
            return Ok(());
        };
        let sym = format!("__lake_die_msg_{func_name}_{tag}");
        let msg_data_id =
            ctx.module_mut()
                .declare_data(&sym, Linkage::Local, false, false)?;
        let mut msg_desc = DataDescription::new();
        msg_desc.define(msg.as_bytes().to_vec().into_boxed_slice());
        ctx.module_mut().define_data(msg_data_id, &msg_desc)?;
        let msg_gv = ctx
            .module_mut()
            .declare_data_in_func(msg_data_id, &mut builder.func);
        let msg_ptr = builder.ins().global_value(ty, msg_gv);
        let msg_len = builder.ins().iconst(ty, msg.len() as i64);
        let sys_write = builder.ins().iconst(ty, 1); // Linux x86-64 SYS_WRITE
        let stderr_fd = builder.ins().iconst(ty, 2);
        let zero_arg = builder.ins().iconst(ty, 0);
        builder.ins().call(
            syscall_ref,
            &[sys_write, stderr_fd, msg_ptr, msg_len, zero_arg, zero_arg, zero_arg],
        );
        Ok(())
    };

    emit_log(
        &mut builder,
        &mut ctx,
        "actor",
        "lake: actor died — rt_allocate: heap exhausted\n",
    )?;

    // Return null fat-ptr.  Caller's next quantum boundary observes
    // IS_DYING and bails to STOP_DONE.
    let null = builder.ins().iconst(ty, 0);
    builder.ins().return_(&[null]);

    // ── init_exit_block: no actor to mark; this is a fatal init-time
    //    allocation failure.  Best we can do is log + exit cleanly
    //    instead of trapping with SIGILL.
    builder.switch_to_block(init_exit_block);
    builder.seal_block(init_exit_block);
    let syscall_id_for_exit = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_allocate init-exit path")),
    };
    let syscall_ref_for_exit = ctx
        .module_mut()
        .declare_func_in_func(syscall_id_for_exit, &mut builder.func);
    emit_log(
        &mut builder,
        &mut ctx,
        "init",
        "lake: init failed — rt_allocate exhausted before scheduler ready\n",
    )?;
    let sys_exit = builder.ins().iconst(ty, 60); // Linux x86-64 SYS_EXIT
    let code = builder.ins().iconst(ty, 137); // 128 + SIGKILL convention
    let zero_arg2 = builder.ins().iconst(ty, 0);
    builder.ins().call(
        syscall_ref_for_exit,
        &[sys_exit, code, zero_arg2, zero_arg2, zero_arg2, zero_arg2, zero_arg2],
    );
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    builder.switch_to_block(cont_block);
    builder.seal_block(cont_block);
    // Suppress unused-import warning when no other site references TrapCode.
    let _ = TrapCode::HEAP_OUT_OF_BOUNDS;

    // Write the fat-pointer header at heap_curr_addr.
    builder
        .ins()
        .store(MemFlags::trusted(), aligned_user_ptr, heap_curr_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), end_addr, heap_curr_addr, 8);

    // Advance heap_curr to end_addr.
    builder
        .ins()
        .store(MemFlags::trusted(), end_addr, heap_curr_ptr, 0);

    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(heap_curr_addr)]);

    // ── huge_block: direct mmap for sizes > 16 MiB ──────────────────────────
    // Layout of the mmap region:
    //   [16 B fat-ptr {start, end}]  [16 B align pad]  [user payload]  [tail to page]
    //                ▲                                ▲
    //             fat_ptr_addr                    fat_ptr.start (= addr + 32)
    // fat_ptr.end = mmap_addr + mmap_size — this lets `rt_free` recover the
    // exact mmap_size as `end - fat_ptr_addr` to pass to `rt_munmap`.
    builder.switch_to_block(huge_block);
    builder.seal_block(huge_block);

    // mmap_size = round_up(user_size + 32, 4096)
    let pre_payload = builder.ins().iconst(ty, 32);
    let total = builder.ins().iadd(user_size, pre_payload);
    let page_minus_one = builder.ins().iconst(ty, 4095);
    let page_mask = builder.ins().iconst(ty, !4095i64);
    let total_round = builder.ins().iadd(total, page_minus_one);
    let mmap_size = builder.ins().band(total_round, page_mask);

    let call_mmap = builder.ins().call(mmap_ref, &[mmap_size]);
    let mmap_addr = builder.inst_results(call_mmap)[0];

    let payload_start = builder.ins().iadd(mmap_addr, pre_payload);
    let payload_end = builder.ins().iadd(mmap_addr, mmap_size);

    builder
        .ins()
        .store(MemFlags::trusted(), payload_start, mmap_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), payload_end, mmap_addr, 8);

    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(mmap_addr)]);

    // ── merge_block: return the fat-pointer address ─────────────────────────
    builder.switch_to_block(merge_block);
    builder.seal_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function(func_name, Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_free(fat_ptr_addr: i64)`.
///
/// Reads the fat-pointer header to determine size and dispatches:
///   * **In-range (bucket ≤ 20, ≤ 16 MiB):** prepend to `free_list[bucket]`.
///     The intrusive next-pointer is stored at offset 0 of the payload.
///   * **Huge (bucket > 20):** call `rt_munmap` on the entire mmap region.
///     `mmap_size = end - fat_ptr_addr` recovers the original mapping span
///     (see `define_allocate` huge_block layout).  The fat-pointer struct
///     itself lives at the start of the mapping, so it disappears with the
///     unmap — no separate free-list push needed.
pub fn define_free(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let free_list_id = match ctx.module().get_name("free_list_heads") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "free_list_heads must be declared before rt_free"
            ));
        }
    };

    let munmap_id = match ctx.module().get_name("rt_munmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_munmap must be declared before rt_free")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let fat_ptr_addr = builder.block_params(entry)[0];

    let free_list_gv = ctx
        .module_mut()
        .declare_data_in_func(free_list_id, &mut builder.func);
    let free_list_ptr = builder.ins().global_value(ty, free_list_gv);

    let munmap_ref = ctx
        .module_mut()
        .declare_func_in_func(munmap_id, &mut builder.func);

    // Read fat_ptr.start and fat_ptr.end to compute payload size.
    let payload_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), fat_ptr_addr, 0);
    let payload_end = builder
        .ins()
        .load(ty, MemFlags::trusted(), fat_ptr_addr, 8);
    let size = builder.ins().isub(payload_end, payload_start);

    // bucket_idx = ceil(log2(max(size, 16))) - 4
    let sixteen = builder.ins().iconst(ty, 16);
    let lt_min = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, size, sixteen);
    let size_clamped = builder.ins().select(lt_min, sixteen, size);
    let size_minus_one = builder.ins().iadd_imm(size_clamped, -1);
    let lz = builder.ins().clz(size_minus_one);
    let total_bits = builder.ins().iconst(ty, 64);
    let log2_ceil = builder.ins().isub(total_bits, lz);
    let bucket_idx = builder.ins().iadd_imm(log2_ceil, -4);

    let max_bucket = builder.ins().iconst(ty, 20);
    let in_range = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bucket_idx, max_bucket);

    let push_block = builder.create_block();
    let huge_free_block = builder.create_block();
    builder
        .ins()
        .brif(in_range, push_block, &[], huge_free_block, &[]);

    // ── push_block: prepend fat_ptr_addr to free_list[bucket] ────────────────
    builder.switch_to_block(push_block);
    builder.seal_block(push_block);
    let bucket_byte_offset = builder.ins().imul_imm(bucket_idx, 8);
    let head_addr = builder.ins().iadd(free_list_ptr, bucket_byte_offset);
    let old_head = builder.ins().load(ty, MemFlags::trusted(), head_addr, 0);
    // Store old_head at payload[0] — this is the chain pointer.
    builder
        .ins()
        .store(MemFlags::trusted(), old_head, payload_start, 0);
    // Update head = fat_ptr_addr.
    builder
        .ins()
        .store(MemFlags::trusted(), fat_ptr_addr, head_addr, 0);
    builder.ins().return_(&[]);

    // ── huge_free_block: munmap the whole region ────────────────────────────
    // mmap_size = end - fat_ptr_addr (see allocate huge_block layout).  The
    // fat-pointer struct lives at the start of the mapping, so a single
    // munmap releases header + payload together.
    builder.switch_to_block(huge_free_block);
    builder.seal_block(huge_free_block);
    let mmap_size = builder.ins().isub(payload_end, fat_ptr_addr);
    builder
        .ins()
        .call(munmap_ref, &[fat_ptr_addr, mmap_size]);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_free", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_store(fat_ptr, val, size, offset)` with bounds checking.
pub fn define_store(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    for _ in 0..4 {
        builder.func.signature.params.push(AbiParam::new(ty));
    }

    let entry = builder.create_block();
    for _ in 0..4 {
        builder.append_block_param(entry, ty);
    }
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let params = builder.block_params(entry);
    let (fat_ptr, val, size, offset) = (params[0], params[1], params[2], params[3]);

    let start = FatPtrLayout::load_start(&mut builder, ty, fat_ptr);
    let end = FatPtrLayout::load_end(&mut builder, ty, fat_ptr);
    let access_ptr = builder.ins().iadd(start, offset);
    let access_end = builder.ins().iadd(access_ptr, size);

    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
    builder
        .ins()
        .trapz(in_bounds, TrapCode::unwrap_user(32));

    // Byte-by-byte store loop — write the LSB-first `size` bytes of
    // `val` to `access_ptr`.  Cranelift's plain `store` always emits
    // the full value-type width (i64 = 8 bytes) and would overrun
    // the caller's intended byte count, clobbering adjacent heap
    // headers.  set_be32's `rt_store(buf, byte, 1, off)` for offsets
    // near a 32-byte buffer's end was silently overwriting the
    // following allocation's fat-ptr.start field — observed in the
    // SHA-256 test (sha_min8) where 7+ sequential set_be32 calls
    // corrupted the spawned actor's exec_ctx and produced &main as
    // a JUMP_ARGS fat-ptr.  The loop honours `size` at runtime.
    let bloop_h = builder.create_block();
    let bloop_b = builder.create_block();
    let bret = builder.create_block();
    builder.append_block_param(bloop_h, ty);
    let zero = builder.ins().iconst(ty, 0);
    builder
        .ins()
        .jump(bloop_h, &[cranelift::codegen::ir::BlockArg::Value(zero)]);

    builder.switch_to_block(bloop_h);
    let i = builder.block_params(bloop_h)[0];
    let cond = builder.ins().icmp(IntCC::UnsignedLessThan, i, size);
    builder.ins().brif(cond, bloop_b, &[], bret, &[]);

    builder.switch_to_block(bloop_b);
    builder.seal_block(bloop_b);
    let shift_bits = builder.ins().imul_imm(i, 8);
    let shifted = builder.ins().ushr(val, shift_bits);
    let byte = builder.ins().ireduce(cranelift::prelude::types::I8, shifted);
    let dst = builder.ins().iadd(access_ptr, i);
    builder.ins().store(MemFlags::new(), byte, dst, 0);
    let i_next = builder.ins().iadd_imm(i, 1);
    builder
        .ins()
        .jump(bloop_h, &[cranelift::codegen::ir::BlockArg::Value(i_next)]);
    builder.seal_block(bloop_h);

    builder.switch_to_block(bret);
    builder.seal_block(bret);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_store", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_load_u{8,16,32,64}(fat_ptr, offset) -> value` for each bit width.
pub fn define_loads(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    for bits in [8u32, 16, 32, 64] {
        let loaded_ty = Type::int(bits as u16).unwrap();

        let mut builder_ctx = FunctionBuilderContext::new();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

        builder.func.signature.params.push(AbiParam::new(ptr_ty));
        builder.func.signature.params.push(AbiParam::new(ptr_ty));
        // Always return i64 so callers can store the value in any
        // i64-sized slot (TEMP_VAL, vars[], call args).  The narrow
        // load is widened in the body via uextend; the rt_registry
        // signature side already advertises `i64` as the return type
        // so the surface and ABI match.
        builder.func.signature.returns.push(AbiParam::new(ptr_ty));

        let entry = builder.create_block();
        builder.append_block_param(entry, ptr_ty);
        builder.append_block_param(entry, ptr_ty);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let [fat_ptr, offset] = builder.block_params(entry)[0..2] else {
            unreachable!()
        };

        let size = builder.ins().iconst(ptr_ty, (bits / 8) as i64);
        let start = FatPtrLayout::load_start(&mut builder, ptr_ty, fat_ptr);
        let end = FatPtrLayout::load_end(&mut builder, ptr_ty, fat_ptr);
        let access_ptr = builder.ins().iadd(start, offset);
        let access_end = builder.ins().iadd(access_ptr, size);

        let in_bounds = builder
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
        builder
            .ins()
            .trapz(in_bounds, TrapCode::unwrap_user(32));

        let raw_val = builder
            .ins()
            .load(loaded_ty, MemFlags::new(), access_ptr, 0);
        // Widen to ptr_ty (i64) when narrower; for the 64-bit case
        // uextend would error, so just pass through.
        let val = if bits < 64 {
            builder.ins().uextend(ptr_ty, raw_val)
        } else {
            raw_val
        };
        builder.ins().return_(&[val]);

        let sig = builder.func.signature.clone();
        let name = format!("rt_load_u{bits}");
        let id = ctx
            .module_mut()
            .declare_function(&name, Linkage::Export, &sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;
        ctx.module_mut().clear_context(&mut module_ctx);
    }

    Ok(ctx)
}

/// Build `rt_copy_bytes(dst_fat, dst_off, src_fat, src_off, len) -> {}`.
///
/// Bounds-checked byte-by-byte memcpy.  Used by stdlib helpers that
/// build a result buffer from one or more source slices (string
/// concat, response builders, hash digest assembly, …).  Both ranges
/// are validated against their respective fat-pointer end markers
/// before the copy starts; an out-of-range access traps.
///
/// The inner loop is a Cranelift loop with one byte per iteration.
/// LLVM-style memcpy intrinsics aren't available in the embedded
/// build, but Cranelift's regalloc + CSE keep this within ~3
/// instructions per byte at -O.  When the language gains a
/// dedicated `buf` type we can switch the hot path to a wider
/// load / store stride.
pub fn define_copy_bytes(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    use cranelift::codegen::ir::BlockArg;
    let ptr_ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    for _ in 0..5 {
        builder.func.signature.params.push(AbiParam::new(ptr_ty));
    }

    // ── Blocks ───────────────────────────────────────────────────────────────
    let entry = builder.create_block();
    for _ in 0..5 {
        builder.append_block_param(entry, ptr_ty);
    }
    let loop_hdr = builder.create_block();
    builder.append_block_param(loop_hdr, ptr_ty); // i (counter)
    let loop_body = builder.create_block();
    let exit = builder.create_block();

    // ── entry ────────────────────────────────────────────────────────────────
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let params = builder.block_params(entry);
    let (dst_fat, dst_off, src_fat, src_off, len) =
        (params[0], params[1], params[2], params[3], params[4]);

    let dst_start = FatPtrLayout::load_start(&mut builder, ptr_ty, dst_fat);
    let dst_end = FatPtrLayout::load_end(&mut builder, ptr_ty, dst_fat);
    let src_start = FatPtrLayout::load_start(&mut builder, ptr_ty, src_fat);
    let src_end = FatPtrLayout::load_end(&mut builder, ptr_ty, src_fat);

    // dst_access_end = dst_start + dst_off + len
    let dst_access_base = builder.ins().iadd(dst_start, dst_off);
    let dst_access_end = builder.ins().iadd(dst_access_base, len);
    let src_access_base = builder.ins().iadd(src_start, src_off);
    let src_access_end = builder.ins().iadd(src_access_base, len);

    let dst_in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, dst_access_end, dst_end);
    builder
        .ins()
        .trapz(dst_in_bounds, TrapCode::unwrap_user(33));
    let src_in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, src_access_end, src_end);
    builder
        .ins()
        .trapz(src_in_bounds, TrapCode::unwrap_user(34));

    let zero = builder.ins().iconst(ptr_ty, 0);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(zero)]);

    // ── loop_hdr (i) ─────────────────────────────────────────────────────────
    builder.switch_to_block(loop_hdr);
    let i = builder.block_params(loop_hdr)[0];
    let cont = builder.ins().icmp(IntCC::UnsignedLessThan, i, len);
    builder.ins().brif(cont, loop_body, &[], exit, &[]);

    // ── loop_body ────────────────────────────────────────────────────────────
    builder.switch_to_block(loop_body);
    builder.seal_block(loop_body);
    let src_addr = builder.ins().iadd(src_access_base, i);
    let byte = builder
        .ins()
        .load(cranelift::prelude::types::I8, MemFlags::new(), src_addr, 0);
    let dst_addr = builder.ins().iadd(dst_access_base, i);
    builder.ins().store(MemFlags::new(), byte, dst_addr, 0);
    let next_i = builder.ins().iadd_imm(i, 1);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(next_i)]);
    builder.seal_block(loop_hdr);

    // ── exit ─────────────────────────────────────────────────────────────────
    builder.switch_to_block(exit);
    builder.seal_block(exit);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_copy_bytes", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}
