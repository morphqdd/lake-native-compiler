use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
        Type, Value,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::pure_expr::atom_id,
    rt::layout::{
        ExecCtxLayout, FatPtrLayout, process_ctx::ProcessCtxLayout, sheduler_ctx::ShedulerCtxLayout,
        slab::SlabLayout,
    },
    target::LinuxSyscalls,
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
        (Some(FuncOrDataId::Data(c)), Some(FuncOrDataId::Data(e)), Some(FuncOrDataId::Data(f))) => {
            (c, e, f)
        }
        _ => {
            return Err(anyhow!(
                "Heap globals + free_list_heads must be declared before rt_allocate"
            ));
        }
    };

    let mmap_id = match ctx.module().get_name("rt_mmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_mmap must be declared before rt_allocate")),
    };

    // #150 phase 4 — read once at lakec invocation.  When set,
    // rt_allocate / rt_allocate_raw route small allocations (class ≤ 11,
    // user_size + 16 ≤ 32 KiB) through `rt_allocate_slab` so the slab
    // allocator's page-reclaim path closes the structural RSS leak.
    // Allocations whose class would exceed 11 still fall through to the
    // existing huge_block direct-mmap path.  See
    // docs/state/features/150_allocator_rewrite.md → Phase plan / phase 4.
    let slab_mode = std::env::var("LAKE_SLAB_ALLOC")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let slab_alloc_id = if slab_mode {
        match ctx.module().get_name("rt_allocate_slab") {
            Some(FuncOrDataId::Func(id)) => Some(id),
            _ => {
                return Err(anyhow!(
                    "rt_allocate_slab must be declared before rt_allocate when LAKE_SLAB_ALLOC=1"
                ));
            }
        }
    } else {
        None
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

    // #150 phase 4 — slab routing.  When LAKE_SLAB_ALLOC=1, small
    // allocations route to `rt_allocate_slab(slab_class_idx)` instead
    // of the freelist/bump path.  `slab_class_idx` is computed from
    // `user_size + 16` (header + payload must fit in one slab chunk),
    // capped at class 11 (= 32 KiB) — anything bigger falls through
    // to the existing huge_block direct-mmap path until phase 5 ships
    // per-class oversized slabs.  Default (slab_mode off) keeps the
    // bucket path verbatim — bit-identical to the pre-phase-4 binary.
    if let Some(slab_alloc_id) = slab_alloc_id {
        // slab_class_idx = ceil(log2(max(user_size + 16, 16))) - 4
        let plus16 = builder.ins().iadd_imm(user_size, 16);
        let slab_lt_min = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, plus16, sixteen);
        let slab_size_clamped = builder.ins().select(slab_lt_min, sixteen, plus16);
        let slab_size_minus_one = builder.ins().iadd_imm(slab_size_clamped, -1);
        let slab_lz = builder.ins().clz(slab_size_minus_one);
        let slab_log2_ceil = builder.ins().isub(total_bits, slab_lz);
        let slab_class_idx = builder.ins().iadd_imm(slab_log2_ceil, -4);

        // Phase 5: slab handles every supported class (0..20 = ≤ 16 MiB).
        // Larger requests fall through to direct-mmap huge_block.
        // Combined with rt_free_slab's no-munmap-on-empty trade-off
        // (kept slabs reused indefinitely), this gives bucket-allocator-
        // like reuse without per-alloc syscalls.
        let max_slab_class = builder.ins().iconst(ty, (SlabLayout::NUM_CLASSES - 1) as i64);
        let slab_supported = builder.ins().icmp(
            IntCC::UnsignedLessThanOrEqual,
            slab_class_idx,
            max_slab_class,
        );

        let slab_alloc_block = builder.create_block();
        builder
            .ins()
            .brif(slab_supported, slab_alloc_block, &[], huge_block, &[]);

        builder.switch_to_block(slab_alloc_block);
        builder.seal_block(slab_alloc_block);

        let slab_alloc_ref = ctx
            .module_mut()
            .declare_func_in_func(slab_alloc_id, &mut builder.func);
        let call = builder.ins().call(slab_alloc_ref, &[slab_class_idx]);
        let chunk = builder.inst_results(call)[0];

        // chunk == 0 → OOM (slab path already marked actor dying).  Mirror
        // the existing OOM tuple/null return.  For tuple ABI, build the
        // `{:err :nomem}` tuple; for raw, just return null.
        let chunk_ok = builder.ins().icmp_imm(IntCC::NotEqual, chunk, 0);
        let write_hdr_block = builder.create_block();
        let slab_oom_block = builder.create_block();
        builder
            .ins()
            .brif(chunk_ok, write_hdr_block, &[], slab_oom_block, &[]);

        // ── write_hdr_block: lay out fat-ptr header at chunk[0..16].
        //    payload_start = chunk + 16,  payload_end = chunk + 16 +
        //    user_size (NOT class_size — keeps bug-122 semantics for the
        //    eventual `rt_free` path).
        builder.switch_to_block(write_hdr_block);
        builder.seal_block(write_hdr_block);
        let payload_start = builder.ins().iadd_imm(chunk, 16);
        let payload_end = builder.ins().iadd(payload_start, user_size);
        builder
            .ins()
            .store(MemFlags::trusted(), payload_start, chunk, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), payload_end, chunk, 8);
        builder.ins().jump(merge_block, &[BlockArg::Value(chunk)]);

        // ── slab_oom_block: OOM in slab path.
        builder.switch_to_block(slab_oom_block);
        builder.seal_block(slab_oom_block);
        if func_name == "rt_allocate" {
            // Tuple-ABI: allocate a 16-byte {atom buf} tuple via rt_allocate_raw
            // and stuff {:err :nomem} into it.  rt_allocate_raw, with
            // LAKE_SLAB_ALLOC=1, also routes through the slab path for the
            // 16-byte tuple — which is class 1 after +16 bump, still well
            // within the slab.
            let alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => {
                    return Err(anyhow!(
                        "rt_allocate_raw must be defined before rt_allocate (slab OOM tuple)"
                    ));
                }
            };
            let alloc_raw_ref = ctx
                .module_mut()
                .declare_func_in_func(alloc_raw_id, &mut builder.func);
            let sixteen_v = builder.ins().iconst(ty, 16);
            let call = builder.ins().call(alloc_raw_ref, &[sixteen_v]);
            let tuple_fp = builder.inst_results(call)[0];
            let tuple_start = builder.ins().load(ty, MemFlags::trusted(), tuple_fp, 0);
            let err_a = builder.ins().iconst(ty, atom_id("err"));
            let nomem_a = builder.ins().iconst(ty, atom_id("nomem"));
            builder
                .ins()
                .store(MemFlags::trusted(), err_a, tuple_start, 0);
            builder
                .ins()
                .store(MemFlags::trusted(), nomem_a, tuple_start, 8);
            builder.ins().return_(&[tuple_fp]);
        } else {
            let null = builder.ins().iconst(ty, 0);
            builder.ins().return_(&[null]);
        }

        // Emit the dead bucket blocks so cranelift's verifier is happy:
        // try_pop_block / pop_block / bump_block / huge_block + the
        // shared merge_block must all be reachable / terminated.  We
        // keep huge_block live (slab_supported = false jumps here) so
        // the existing huge-mmap emission below handles classes 12+.
        //
        // try_pop_block / pop_block / bump_block are unused in slab
        // mode — terminate them with a trap so they're well-formed.
        builder.switch_to_block(try_pop_block);
        builder.seal_block(try_pop_block);
        builder.ins().trap(TrapCode::unwrap_user(0xD0));
        builder.switch_to_block(pop_block);
        builder.seal_block(pop_block);
        builder.ins().trap(TrapCode::unwrap_user(0xD0));
        builder.switch_to_block(bump_block);
        builder.seal_block(bump_block);
        builder.ins().trap(TrapCode::unwrap_user(0xD0));

        // Now switch to huge_block; the post-bucket emission below
        // (huge_block body → merge_block → return) will fill it in
        // verbatim.  We deliberately do NOT re-emit any of the bucket
        // / OOM logic below for slab mode — the early-returns above
        // already covered every reachable path.
        //
        // Note: the code that follows still references `heap_curr_addr`
        // etc.  We need to skip past that.  Use an early-return flag
        // pattern by jumping directly to huge_block emission.
        builder.switch_to_block(huge_block);
        builder.seal_block(huge_block);

        // Fall through into the huge_block IR emitted below by
        // restoring the standard layout: huge_block body lives at the
        // `// ── huge_block: direct mmap` section.  We jump there
        // via the natural code flow (huge_block is the next block we
        // switched into, and the subsequent emission writes the
        // mmap-path instructions into it).  Therefore we MUST suppress
        // the intervening bump-path emission.  Use a sentinel branch.
        //
        // Implementation: emit a small unreachable block to absorb the
        // bump-path / OOM emission, then re-position to huge_block for
        // the mmap emission, then jump merge_block → return.
        //
        // Simpler: just emit the slab-mode-specific huge_block + return
        // inline here, and `return Ok(ctx)` to skip the bucket emission
        // entirely.

        // ── huge_block: direct mmap for sizes > 32 KiB ──────────────
        // Same layout as bucket-mode huge_block: 16 B fat-ptr header,
        // 16 B alignment pad, user payload, page-rounded tail.
        let pre_payload = builder.ins().iconst(ty, 32);
        let total = builder.ins().iadd(user_size, pre_payload);
        let page_minus_one = builder.ins().iconst(ty, 4095);
        let page_mask = builder.ins().iconst(ty, !4095i64);
        let total_round = builder.ins().iadd(total, page_minus_one);
        let mmap_size = builder.ins().band(total_round, page_mask);

        let call_mmap = builder.ins().call(mmap_ref, &[mmap_size]);
        let mmap_addr = builder.inst_results(call_mmap)[0];

        let huge_payload_start = builder.ins().iadd(mmap_addr, pre_payload);
        let huge_payload_end = builder.ins().iadd(mmap_addr, mmap_size);

        builder
            .ins()
            .store(MemFlags::trusted(), huge_payload_start, mmap_addr, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), huge_payload_end, mmap_addr, 8);
        builder
            .ins()
            .jump(merge_block, &[BlockArg::Value(mmap_addr)]);

        // ── merge_block: return the fat-pointer (with tuple wrap for
        //    user-facing rt_allocate).  Duplicates the bucket-mode merge
        //    logic so we can emit `return Ok(ctx)` cleanly here.
        builder.switch_to_block(merge_block);
        builder.seal_block(merge_block);
        let result = builder.block_params(merge_block)[0];

        if func_name == "rt_allocate" {
            // Tuple wrapper {atom buf} via arena — reclaimed on actor
            // death, eliminating the 16-byte-per-call leak previously
            // observed with rt_allocate_raw.
            let arena_alloc_id = match ctx.module().get_name("rt_arena_alloc") {
                Some(FuncOrDataId::Func(id)) => id,
                _ => {
                    return Err(anyhow!(
                        "rt_arena_alloc missing for rt_allocate tuple wrap (slab)"
                    ));
                }
            };
            let arena_alloc_ref = ctx
                .module_mut()
                .declare_func_in_func(arena_alloc_id, &mut builder.func);
            let sixteen_v = builder.ins().iconst(ty, 16);
            let call = builder.ins().call(arena_alloc_ref, &[sixteen_v]);
            let tuple_fp = builder.inst_results(call)[0];
            let tuple_start = builder.ins().load(ty, MemFlags::trusted(), tuple_fp, 0);
            let ok_a = builder.ins().iconst(ty, atom_id("ok"));
            builder
                .ins()
                .store(MemFlags::trusted(), ok_a, tuple_start, 0);
            builder
                .ins()
                .store(MemFlags::trusted(), result, tuple_start, 8);
            builder.ins().return_(&[tuple_fp]);
        } else {
            builder.ins().return_(&[result]);
        }

        let sig = builder.func.signature.clone();
        let id = ctx
            .module_mut()
            .declare_function(func_name, Linkage::Export, &sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;
        ctx.module_mut().clear_context(&mut module_ctx);
        return Ok(ctx);
    }

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
    let next = builder.ins().load(ty, MemFlags::trusted(), payload_addr, 0);
    // free_list[bucket] = next
    builder.ins().store(MemFlags::trusted(), next, head_addr, 0);

    // Rewrite the recycled fat-ptr's end to user_size (see #122).  The prior
    // allocation left this as bucket-rounded — would over-report size().
    let visible_end_pop = builder.ins().iadd(payload_addr, user_size);
    builder
        .ins()
        .store(MemFlags::trusted(), visible_end_pop, head, 8);

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
        builder.ins().jump(zero_hdr, &[BlockArg::Value(zero_start)]);

        builder.switch_to_block(zero_hdr);
        let zi = builder.block_params(zero_hdr)[0];
        let zcont = builder.ins().icmp(IntCC::UnsignedLessThan, zi, zero_limit);
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
        builder.ins().jump(merge_block, &[BlockArg::Value(head)]);
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
    let heap_end_addr = builder.ins().load(ty, MemFlags::trusted(), heap_end_ptr, 0);

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

    // ── oom_block: behaviour depends on which entry point this is.
    //    `rt_allocate` is the user-facing tuple-ABI variant: return
    //    `{:err :nomem}` and let the lowering pass's bare-call wrapper
    //    (or an explicit user `when r.0 { :err -> ... }`) decide whether
    //    the actor dies.  `rt_allocate_raw` is the scheduler-internal
    //    path used by spawn / tuple_expr / io_uring init; mark IS_DYING
    //    (or process-exit at init time) and return null.
    builder.switch_to_block(oom_block);
    builder.seal_block(oom_block);

    let is_tuple_abi = func_name == "rt_allocate";

    if is_tuple_abi {
        // Allocate the 2-slot tuple `{atom buf}` via the raw allocator
        // (already defined, courtesy of the init-order swap).  Write
        // `:err :nomem`, return the tuple's fat-ptr.  No IS_DYING set
        // here — the lowering pass injects `rt_die_actor()` at bare
        // call sites; let-bound callers can recover.
        let alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => {
                return Err(anyhow!(
                    "rt_allocate_raw must be defined before rt_allocate (tuple ABI)"
                ));
            }
        };
        let alloc_raw_ref = ctx
            .module_mut()
            .declare_func_in_func(alloc_raw_id, &mut builder.func);
        let sixteen = builder.ins().iconst(ty, 16);
        let call = builder.ins().call(alloc_raw_ref, &[sixteen]);
        let tuple_fp = builder.inst_results(call)[0];
        let tuple_start = builder.ins().load(ty, MemFlags::trusted(), tuple_fp, 0);
        let err_a = builder.ins().iconst(ty, atom_id("err"));
        let nomem_a = builder.ins().iconst(ty, atom_id("nomem"));
        builder
            .ins()
            .store(MemFlags::trusted(), err_a, tuple_start, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), nomem_a, tuple_start, 8);
        builder.ins().return_(&[tuple_fp]);

        // Move to cont_block so the shared happy-path emission below
        // can write the fat-pointer header straight into it.
        builder.switch_to_block(cont_block);
        builder.seal_block(cont_block);
        let _ = TrapCode::HEAP_OUT_OF_BOUNDS;
    } else {
        let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
            Some(FuncOrDataId::Data(id)) => id,
            _ => {
                return Err(anyhow!(
                    "sheduler_ctx_fat_ptr global not found for OOM die path"
                ));
            }
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
        let proc_arr_start = builder.ins().load(ty, MemFlags::trusted(), proc_arr_fat, 0);
        let current_idx = builder.ins().load(
            ty,
            MemFlags::trusted(),
            sched_ptr,
            ShedulerCtxLayout::CURRENT_PROCESS,
        );
        let idx_scaled = builder.ins().imul_imm(current_idx, 8);
        let slot_addr = builder.ins().iadd(proc_arr_start, idx_scaled);
        let proc_ctx_fat = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 0);
        let proc_ctx_ptr = builder.ins().load(ty, MemFlags::trusted(), proc_ctx_fat, 0);
        let exec_ctx_fat = builder.ins().load(
            ty,
            MemFlags::trusted(),
            proc_ctx_ptr,
            ProcessCtxLayout::EXEC_CTX,
        );
        let exec_ctx_ptr = builder.ins().load(ty, MemFlags::trusted(), exec_ctx_fat, 0);
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
            let msg_data_id = ctx
                .module_mut()
                .declare_data(&sym, Linkage::Local, false, false)?;
            let mut msg_desc = DataDescription::new();
            msg_desc.define(msg.as_bytes().to_vec().into_boxed_slice());
            ctx.module_mut().define_data(msg_data_id, &msg_desc)?;
            let msg_gv = ctx
                .module_mut()
                .declare_data_in_func(msg_data_id, &mut builder.func);
            let msg_ptr = builder.ins().global_value(ty, msg_gv);
            let msg_len = builder.ins().iconst(ty, msg.len() as i64);
            let sys_write = builder
                .ins()
                .iconst(ty, ctx.syscalls().sys_write);
            let stderr_fd = builder.ins().iconst(ty, 2);
            let zero_arg = builder.ins().iconst(ty, 0);
            builder.ins().call(
                syscall_ref,
                &[
                    sys_write, stderr_fd, msg_ptr, msg_len, zero_arg, zero_arg, zero_arg,
                ],
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
            _ => {
                return Err(anyhow!(
                    "rt_syscall must be declared before rt_allocate init-exit path"
                ));
            }
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
        let sys_exit = builder.ins().iconst(ty, ctx.syscalls().sys_exit);
        let code = builder.ins().iconst(ty, 137); // 128 + SIGKILL convention
        let zero_arg2 = builder.ins().iconst(ty, 0);
        builder.ins().call(
            syscall_ref_for_exit,
            &[
                sys_exit, code, zero_arg2, zero_arg2, zero_arg2, zero_arg2, zero_arg2,
            ],
        );
        builder.ins().trap(TrapCode::user(0xDE).unwrap());

        builder.switch_to_block(cont_block);
        builder.seal_block(cont_block);
    } // end of raw OOM `else` branch — builder is now on cont_block.
    // Suppress unused-import warning when no other site references TrapCode.
    let _ = TrapCode::HEAP_OUT_OF_BOUNDS;

    // Write the fat-pointer header at heap_curr_addr.
    // See docs/state/bugs/122_alloc_or_die_size_capacity.md: fat-ptr end
    // reflects the user-requested length, not the bucket-rounded capacity.
    let visible_end = builder.ins().iadd(aligned_user_ptr, user_size);
    builder
        .ins()
        .store(MemFlags::trusted(), aligned_user_ptr, heap_curr_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), visible_end, heap_curr_addr, 8);

    // Advance heap_curr to end_addr (bucket-rounded — bookkeeping only).
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
    // TODO #122: still bucket-rounded — size(b) for >16 MiB returns the
    // page-aligned mmap_size, not user_size.  Fix when rt_free learns to
    // recover mmap_size from a header word instead of `end - fat_ptr`.
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

    if is_tuple_abi {
        // Wrap the buf fat-ptr in a 2-slot tuple `{:ok buf}`.  The
        // 16-byte tuple lives in the actor's arena — reclaimed on
        // actor death.  Previously this routed through rt_allocate_raw
        // which had no scheduler-side free, leaking 16 B per
        // rt_allocate call.
        let arena_alloc_id = match ctx.module().get_name("rt_arena_alloc") {
            Some(FuncOrDataId::Func(id)) => id,
            _ => {
                return Err(anyhow!(
                    "rt_arena_alloc missing for rt_allocate tuple wrap"
                ));
            }
        };
        let arena_alloc_ref = ctx
            .module_mut()
            .declare_func_in_func(arena_alloc_id, &mut builder.func);
        let sixteen = builder.ins().iconst(ty, 16);
        let call = builder.ins().call(arena_alloc_ref, &[sixteen]);
        let tuple_fp = builder.inst_results(call)[0];
        let tuple_start = builder.ins().load(ty, MemFlags::trusted(), tuple_fp, 0);
        let ok_a = builder.ins().iconst(ty, atom_id("ok"));
        builder
            .ins()
            .store(MemFlags::trusted(), ok_a, tuple_start, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), result, tuple_start, 8);
        builder.ins().return_(&[tuple_fp]);
    } else {
        builder.ins().return_(&[result]);
    }

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
            return Err(anyhow!("free_list_heads must be declared before rt_free"));
        }
    };

    let munmap_id = match ctx.module().get_name("rt_munmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_munmap must be declared before rt_free")),
    };

    // #150 phase 4 — mirror rt_allocate's compile-time env switch.  When
    // LAKE_SLAB_ALLOC=1, route small chunks through rt_free_slab; class >
    // 11 (= came from huge_block direct mmap) keeps the legacy munmap path.
    let slab_mode = std::env::var("LAKE_SLAB_ALLOC")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let slab_free_id = if slab_mode {
        match ctx.module().get_name("rt_free_slab") {
            Some(FuncOrDataId::Func(id)) => Some(id),
            _ => {
                return Err(anyhow!(
                    "rt_free_slab must be declared before rt_free when LAKE_SLAB_ALLOC=1"
                ));
            }
        }
    } else {
        None
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
    let payload_start = builder.ins().load(ty, MemFlags::trusted(), fat_ptr_addr, 0);
    let payload_end = builder.ins().load(ty, MemFlags::trusted(), fat_ptr_addr, 8);
    let size = builder.ins().isub(payload_end, payload_start);

    // bucket_idx = ceil(log2(max(size, 16))) - 4
    let sixteen = builder.ins().iconst(ty, 16);
    let lt_min = builder.ins().icmp(IntCC::UnsignedLessThan, size, sixteen);
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

    // #150 phase 4: slab-mode free routing.
    if let Some(slab_free_id) = slab_free_id {
        // Recompute class_idx using user_size + 16 so it matches the
        // class chosen at alloc time.  Without the +16 bump, a chunk
        // alloc'd at class C might free at class C-1, mis-routing
        // boundary sizes (e.g. size=16: alloc→class 1 (slab), free→
        // class 0 if we used `size` directly).
        let plus16 = builder.ins().iadd_imm(size, 16);
        let plus16_lt = builder.ins().icmp(IntCC::UnsignedLessThan, plus16, sixteen);
        let plus16_clamped = builder.ins().select(plus16_lt, sixteen, plus16);
        let plus16_minus_one = builder.ins().iadd_imm(plus16_clamped, -1);
        let slab_lz = builder.ins().clz(plus16_minus_one);
        let slab_log2_ceil = builder.ins().isub(total_bits, slab_lz);
        let slab_class_idx = builder.ins().iadd_imm(slab_log2_ceil, -4);

        // Phase 5: free routes ≤ class 20 (≤ 16 MiB) through slab path —
        // matches alloc-side widening.
        let max_slab = builder.ins().iconst(ty, (SlabLayout::NUM_CLASSES - 1) as i64);
        let slab_supported = builder.ins().icmp(
            IntCC::UnsignedLessThanOrEqual,
            slab_class_idx,
            max_slab,
        );

        let slab_free_block = builder.create_block();
        builder
            .ins()
            .brif(slab_supported, slab_free_block, &[], huge_free_block, &[]);

        builder.switch_to_block(slab_free_block);
        builder.seal_block(slab_free_block);
        let slab_free_ref = ctx
            .module_mut()
            .declare_func_in_func(slab_free_id, &mut builder.func);
        // chunk_addr == fat_ptr_addr (slab path lays the 16-byte header
        // at chunk[0..16]).
        builder.ins().call(slab_free_ref, &[fat_ptr_addr]);
        builder.ins().return_(&[]);

        // Orphan the legacy bucket-path block: never reached, but the
        // verifier expects every block to be terminated.
        builder.switch_to_block(push_block);
        builder.seal_block(push_block);
        builder.ins().trap(TrapCode::unwrap_user(0xD1));

        // huge_free_block: emit the legacy munmap path inline here.  We
        // can't fall through to the post-bucket emission because that
        // path also relies on `push_block`'s control flow.
        builder.switch_to_block(huge_free_block);
        builder.seal_block(huge_free_block);
        let mmap_size = builder.ins().isub(payload_end, fat_ptr_addr);
        builder.ins().call(munmap_ref, &[fat_ptr_addr, mmap_size]);
        builder.ins().return_(&[]);

        // Suppress unused warnings on bucket-path locals.
        let _ = (free_list_ptr, in_range, bucket_idx);

        let sig = builder.func.signature.clone();
        let id = ctx
            .module_mut()
            .declare_function("rt_free", Linkage::Export, &sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;
        ctx.module_mut().clear_context(&mut module_ctx);
        return Ok(ctx);
    }

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

    // ── #150: return committed pages to OS for chunks >= 16 KB ───────────
    // The chunk stays linked into the free-list (allocator bookkeeping
    // intact) but its underlying physical pages get released via
    // madvise(MADV_DONTNEED).  RSS drops; next alloc that touches the
    // region faults zero-fill pages back in.
    //
    // Page-alignment: payload_start is 16-byte aligned (not page-aligned)
    // and bytes [payload_start .. +8) hold the free-list chain pointer.
    // We round the advise address UP to the next page boundary that's at
    // or after `payload_start + 8`, then madvise the remaining
    // page-aligned tail (rounded down).  This guarantees:
    //   * chain pointer page is NEVER advised (chain survives)
    //   * advise address + length are page-aligned (madvise's EINVAL)
    // For 64 KB arena allocs (typical): ~56-60 KB returned per free.
    let page_size = builder.ins().iconst(ty, 4096);
    let two_pages = builder.ins().iconst(ty, 8192);
    let bucket_log = builder.ins().iadd_imm(bucket_idx, 4);
    let one_v = builder.ins().iconst(ty, 1);
    let bucket_size = builder.ins().ishl(one_v, bucket_log);
    // Madvise when the chunk spans at least 2 pages so the chain-page
    // skip + alignment slack still leaves at least 1 page to advise.
    let big_enough = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, bucket_size, two_pages);
    let madvise_block = builder.create_block();
    let return_block = builder.create_block();
    builder
        .ins()
        .brif(big_enough, madvise_block, &[], return_block, &[]);

    builder.switch_to_block(madvise_block);
    builder.seal_block(madvise_block);
    // advise_addr = round_up_to_page(payload_start + 8)
    let after_chain = builder.ins().iadd_imm(payload_start, 8);
    let plus_4095 = builder.ins().iadd_imm(after_chain, 4095);
    let page_mask = builder.ins().iconst(ty, !4095i64);
    let advise_addr = builder.ins().band(plus_4095, page_mask);
    // tail_len = (payload_start + bucket_size) - advise_addr, rounded down
    let chunk_end = builder.ins().iadd(payload_start, bucket_size);
    let tail = builder.ins().isub(chunk_end, advise_addr);
    let advise_len = builder.ins().band(tail, page_mask);
    // Guard: if advise_len < page_size, skip (no full page to advise).
    let has_page = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, advise_len, page_size);
    let do_advise_block = builder.create_block();
    builder
        .ins()
        .brif(has_page, do_advise_block, &[], return_block, &[]);

    builder.switch_to_block(do_advise_block);
    builder.seal_block(do_advise_block);
    let madvise_nr = builder.ins().iconst(ty, ctx.syscalls().sys_madvise);
    let madv_dontneed = builder.ins().iconst(ty, 4);
    let z = builder.ins().iconst(ty, 0);
    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_free")),
    };
    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    builder.ins().call(
        syscall_ref,
        &[madvise_nr, advise_addr, advise_len, madv_dontneed, z, z, z],
    );
    builder.ins().jump(return_block, &[]);

    builder.switch_to_block(return_block);
    builder.seal_block(return_block);
    builder.ins().return_(&[]);

    // ── huge_free_block: munmap the whole region ────────────────────────────
    // mmap_size = end - fat_ptr_addr (see allocate huge_block layout).  The
    // fat-pointer struct lives at the start of the mapping, so a single
    // munmap releases header + payload together.
    builder.switch_to_block(huge_free_block);
    builder.seal_block(huge_free_block);
    let mmap_size = builder.ins().isub(payload_end, fat_ptr_addr);
    builder.ins().call(munmap_ref, &[fat_ptr_addr, mmap_size]);
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
    builder.ins().trapz(in_bounds, TrapCode::unwrap_user(32));

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
    let byte = builder
        .ins()
        .ireduce(cranelift::prelude::types::I8, shifted);
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
        builder.ins().trapz(in_bounds, TrapCode::unwrap_user(32));

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

/// Build `rt_arena_alloc(size: i64) -> i64` — feature #138 per-actor
/// arena bump allocator.
///
/// Returns a fat-pointer address (same ABI as `rt_allocate`) pointing
/// at a freshly-carved 16-byte header inside the current actor's
/// arena, followed by `size` bytes of payload.  When the arena is
/// exhausted or absent (e.g. `main`'s init process), falls back to
/// `rt_allocate` so callers see no semantic difference.
///
/// The arena itself is one `rt_allocate_raw(64 KB)` per spawned actor
/// (see `spawn_expr`).  Bump cursor + end live in `proc_ctx` fields
/// `OWNED_ARENA_BUMP` / `OWNED_ARENA_END`.  On actor death,
/// `free_process_resources` calls `rt_free` on the whole arena —
/// every allocation made via this path is reclaimed in one shot,
/// eliminating the user-side "I forgot rt_free()" leak.
pub fn define_arena_alloc(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "sheduler_ctx_fat_ptr must be declared before rt_arena_alloc"
            ));
        }
    };
    let rt_allocate_raw_id = match ctx.module().get_name("rt_allocate_raw") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_allocate_raw must be declared before rt_arena_alloc"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ptr_ty));
    builder.func.signature.returns.push(AbiParam::new(ptr_ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ptr_ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let user_size = builder.block_params(entry)[0];

    // ── Locate the current actor's proc_ctx via the scheduler ──────────
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    let sh_ctx_ptr = builder.ins().global_value(ptr_ty, sched_gv);
    let sh_data = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);
    let cur_idx = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        sh_data,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let proc_arr_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        sh_data,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), proc_arr_fat, 0);
    let entry_off = builder.ins().imul_imm(cur_idx, 8);
    let entry_addr = builder.ins().iadd(proc_arr, entry_off);
    let proc_ctx_fat = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), entry_addr, 0);
    let proc_ctx = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), proc_ctx_fat, 0);

    // Load arena fat-ptr from proc_ctx.  When zero — no arena → fallback.
    let arena_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        proc_ctx,
        ProcessCtxLayout::OWNED_ARENA_FAT,
    );

    let has_arena_block = builder.create_block();
    let fallback_block = builder.create_block();
    let has_arena = builder.ins().icmp_imm(IntCC::NotEqual, arena_fat, 0);
    builder
        .ins()
        .brif(has_arena, has_arena_block, &[], fallback_block, &[]);

    builder.switch_to_block(has_arena_block);
    builder.seal_block(has_arena_block);

    // Bump cursor IS the fat-ptr's `start` field — mutated in-place
    // so child ret-machines that inherit the same fat-ptr (sync
    // spawn, phase 2c) see the same allocator state.
    let bump = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), arena_fat, 0);
    let arena_end = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), arena_fat, 8);

    let plus_seven = builder.ins().iadd_imm(user_size, 7);
    let mask = builder.ins().iconst(ptr_ty, !7i64);
    let aligned_size = builder.ins().band(plus_seven, mask);
    let needed = builder.ins().iadd_imm(aligned_size, 16);
    let new_bump = builder.ins().iadd(bump, needed);

    // Arena exhaustion → fallback.
    let fits = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, new_bump, arena_end);
    let bump_block = builder.create_block();
    builder
        .ins()
        .brif(fits, bump_block, &[], fallback_block, &[]);

    builder.switch_to_block(bump_block);
    builder.seal_block(bump_block);

    // Header layout: [start, end] at `bump`, then `size` payload bytes.
    let payload_start = builder.ins().iadd_imm(bump, 16);
    let payload_end = builder.ins().iadd(payload_start, user_size);
    builder
        .ins()
        .store(MemFlags::trusted(), payload_start, bump, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), payload_end, bump, 8);

    // Advance bump in the fat-ptr's `start` field (shared with all
    // actors that inherited this arena fat-ptr).
    builder
        .ins()
        .store(MemFlags::trusted(), new_bump, arena_fat, 0);

    builder.ins().return_(&[bump]);

    // ── fallback_block: route through legacy bucket allocator ──────────
    builder.switch_to_block(fallback_block);
    builder.seal_block(fallback_block);
    let alloc_raw_ref = ctx
        .module_mut()
        .declare_func_in_func(rt_allocate_raw_id, &mut builder.func);
    let call_alloc = builder.ins().call(alloc_raw_ref, &[user_size]);
    let result = builder.inst_results(call_alloc)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_arena_alloc", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_copy_to_arena(src_fat_ptr, target_proc_ctx) -> i64` — copy
/// the bytes of `src` into `target`'s arena (#138 phase 2d).
///
/// Async spawn argument-copy primitive: when caller A spawns
/// non-ret machine B with a buf argument, A's arena outlive can
/// outpace B's reads.  Copying the bytes into B's arena gives B an
/// independent copy whose lifetime matches its own actor.  Standard
/// actor-model semantics — Erlang's `spawn` copies args between
/// processes for the same reason.
///
/// `target_proc_ctx` is a fat-ptr address to the target actor's
/// proc_ctx (where OWNED_ARENA_FAT lives).  Falls back to
/// `rt_allocate_raw(size)` + memcpy when the target arena is
/// exhausted or absent.
///
/// Returns a fresh fat-ptr address into target's arena, ABI-equivalent
/// to `rt_allocate_raw`.
pub fn define_copy_to_arena(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let rt_allocate_raw_id = match ctx.module().get_name("rt_allocate_raw") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_allocate_raw must be declared before rt_copy_to_arena"
            ));
        }
    };
    let rt_copy_bytes_id = match ctx.module().get_name("rt_copy_bytes") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_copy_bytes must be declared before rt_copy_to_arena"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    // (src_fat_ptr, target_proc_ctx_fat_ptr) -> i64
    builder.func.signature.params.push(AbiParam::new(ptr_ty));
    builder.func.signature.params.push(AbiParam::new(ptr_ty));
    builder.func.signature.returns.push(AbiParam::new(ptr_ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ptr_ty);
    builder.append_block_param(entry, ptr_ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let src_fat = builder.block_params(entry)[0];
    let target_proc_ctx_fat = builder.block_params(entry)[1];

    // size = src.end - src.start
    let src_start = builder.ins().load(ptr_ty, MemFlags::trusted(), src_fat, 0);
    let src_end = builder.ins().load(ptr_ty, MemFlags::trusted(), src_fat, 8);
    let size = builder.ins().isub(src_end, src_start);

    // Resolve target's arena_fat: deref target_proc_ctx, load arena field.
    let target_proc_ctx = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), target_proc_ctx_fat, 0);
    let target_arena_fat = builder.ins().load(
        ptr_ty,
        MemFlags::trusted(),
        target_proc_ctx,
        ProcessCtxLayout::OWNED_ARENA_FAT,
    );

    let has_arena_block = builder.create_block();
    let fallback_block = builder.create_block();
    let has_arena = builder
        .ins()
        .icmp_imm(IntCC::NotEqual, target_arena_fat, 0);
    builder
        .ins()
        .brif(has_arena, has_arena_block, &[], fallback_block, &[]);

    // ── has_arena: bump from target arena ─────────────────────────────
    builder.switch_to_block(has_arena_block);
    builder.seal_block(has_arena_block);

    let bump = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), target_arena_fat, 0);
    let arena_end = builder
        .ins()
        .load(ptr_ty, MemFlags::trusted(), target_arena_fat, 8);
    let plus_seven = builder.ins().iadd_imm(size, 7);
    let mask = builder.ins().iconst(ptr_ty, !7i64);
    let aligned_size = builder.ins().band(plus_seven, mask);
    let needed = builder.ins().iadd_imm(aligned_size, 16);
    let new_bump = builder.ins().iadd(bump, needed);
    let fits = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, new_bump, arena_end);
    let copy_block = builder.create_block();
    builder
        .ins()
        .brif(fits, copy_block, &[], fallback_block, &[]);

    builder.switch_to_block(copy_block);
    builder.seal_block(copy_block);
    let payload_start = builder.ins().iadd_imm(bump, 16);
    let payload_end = builder.ins().iadd(payload_start, size);
    builder
        .ins()
        .store(MemFlags::trusted(), payload_start, bump, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), payload_end, bump, 8);
    builder
        .ins()
        .store(MemFlags::trusted(), new_bump, target_arena_fat, 0);

    // memcpy src → new chunk via rt_copy_bytes(dst_fat, 0, src_fat, 0, size).
    let copy_ref = ctx
        .module_mut()
        .declare_func_in_func(rt_copy_bytes_id, &mut builder.func);
    let zero = builder.ins().iconst(ptr_ty, 0);
    builder
        .ins()
        .call(copy_ref, &[bump, zero, src_fat, zero, size]);
    builder.ins().return_(&[bump]);

    // ── fallback: rt_allocate_raw + memcpy ────────────────────────────
    builder.switch_to_block(fallback_block);
    builder.seal_block(fallback_block);
    let alloc_raw_ref = ctx
        .module_mut()
        .declare_func_in_func(rt_allocate_raw_id, &mut builder.func);
    let call_alloc = builder.ins().call(alloc_raw_ref, &[size]);
    let dst_fat = builder.inst_results(call_alloc)[0];
    let copy_ref_fb = ctx
        .module_mut()
        .declare_func_in_func(rt_copy_bytes_id, &mut builder.func);
    let zero_fb = builder.ins().iconst(ptr_ty, 0);
    builder
        .ins()
        .call(copy_ref_fb, &[dst_fat, zero_fb, src_fat, zero_fb, size]);
    builder.ins().return_(&[dst_fat]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_copy_to_arena", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

// ─────────────────────────────────────────────────────────────────────────────
// #150 phase 2 — slab allocator front door (`rt_allocate_slab`)
//
// Allocates one chunk of the requested size class from a per-class slab.
// The caller pre-computes `class_idx = ceil(log2(max(size, 16))) - 4` and
// passes it in.  Returns a fat-pointer address (16 B header + chunk
// payload), or 0 on OOM (after marking the running actor as dying).
//
// Layout details live in `rt/layout/slab.rs`.  Phase plan + rationale:
// `docs/state/features/150_allocator_rewrite.md`.
// ─────────────────────────────────────────────────────────────────────────────

/// Encode `vals` as a little-endian i64 byte vector.  Used to bake
/// per-class compile-time tables into `.rodata`.
fn i64_le_bytes(vals: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 8);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Compute the chunk-array payload offset for `class_idx`:
/// `align_up(HDR_FIXED_BYTES + ceil(chunks_per_slab / 8), CHUNK_ALIGN)`.
/// Matches the geometry assumed in `SlabLayout::chunks_per_slab`.
fn payload_offset(class_idx: usize) -> usize {
    let chunks = SlabLayout::chunks_per_slab(class_idx);
    if chunks == 0 {
        return 0;
    }
    let bitmap_bytes = chunks.div_ceil(8);
    let align = SlabLayout::CHUNK_ALIGN as usize;
    let raw = SlabLayout::HDR_FIXED_BYTES as usize + bitmap_bytes;
    raw.div_ceil(align) * align
}

/// Number of 64-bit bitmap words covering all chunks in a class's slab.
fn num_words(class_idx: usize) -> usize {
    SlabLayout::chunks_per_slab(class_idx).div_ceil(64)
}

/// Emit IR for `try_alloc_from_slab(slab) -> chunk_addr or 0`.
///
/// Scans the bitmap words of `slab` for the first set bit, clears it,
/// decrements `free_count`, and jumps to `success_block` with the chunk
/// address as the block arg.  If `free_count` was already 0, jumps to
/// `fail_block` with no arg.
///
/// `chunks_per_slab_val`, `payload_offset_val`, `class_size_val`,
/// `num_words_val` are runtime values loaded from the per-class tables.
#[allow(clippy::too_many_arguments)]
fn emit_try_alloc_from_slab(
    builder: &mut FunctionBuilder,
    ty: Type,
    slab: Value,
    chunks_per_slab_val: Value,
    payload_offset_val: Value,
    class_size_val: Value,
    num_words_val: Value,
    success_block: cranelift::prelude::Block,
    fail_block: cranelift::prelude::Block,
) {
    // free_count = *(slab + HDR_FREE_COUNT)
    let free_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        slab,
        SlabLayout::HDR_FREE_COUNT,
    );
    let has_free = builder.ins().icmp_imm(IntCC::NotEqual, free_count, 0);

    // unused warnings dodge
    let _ = chunks_per_slab_val;

    let scan_hdr = builder.create_block();
    builder.append_block_param(scan_hdr, ty); // word_idx
    let found_block = builder.create_block();
    builder.append_block_param(found_block, ty); // word_addr
    builder.append_block_param(found_block, ty); // word_value

    let zero_w = builder.ins().iconst(ty, 0);
    builder
        .ins()
        .brif(has_free, scan_hdr, &[BlockArg::Value(zero_w)], fail_block, &[]);

    // scan_hdr(word_idx): bounds check + load word
    builder.switch_to_block(scan_hdr);
    let widx = builder.block_params(scan_hdr)[0];
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, widx, num_words_val);
    let load_word_block = builder.create_block();
    // No free bit found anywhere — shouldn't happen if free_count > 0, but
    // be defensive: fall through to fail_block.
    builder.ins().brif(in_bounds, load_word_block, &[], fail_block, &[]);

    builder.switch_to_block(load_word_block);
    builder.seal_block(load_word_block);
    let bitmap_base = builder
        .ins()
        .iadd_imm(slab, SlabLayout::HDR_BITMAP_START as i64);
    let word_off = builder.ins().imul_imm(widx, 8);
    let word_addr = builder.ins().iadd(bitmap_base, word_off);
    let word = builder.ins().load(ty, MemFlags::trusted(), word_addr, 0);
    let nonzero = builder.ins().icmp_imm(IntCC::NotEqual, word, 0);
    let widx_next = builder.ins().iadd_imm(widx, 1);
    builder.ins().brif(
        nonzero,
        found_block,
        &[BlockArg::Value(word_addr), BlockArg::Value(word)],
        scan_hdr,
        &[BlockArg::Value(widx_next)],
    );
    builder.seal_block(scan_hdr);

    // found_block(word_addr, word_value): clear bit, decrement count,
    // compute chunk address, jump to success_block.
    builder.switch_to_block(found_block);
    builder.seal_block(found_block);
    let waddr = builder.block_params(found_block)[0];
    let wval = builder.block_params(found_block)[1];
    let bit_idx = builder.ins().ctz(wval);
    let one_v = builder.ins().iconst(ty, 1);
    let bit_mask = builder.ins().ishl(one_v, bit_idx);
    let inv_mask = builder.ins().bnot(bit_mask);
    let new_word = builder.ins().band(wval, inv_mask);
    builder.ins().store(MemFlags::trusted(), new_word, waddr, 0);

    let dec = builder.ins().iadd_imm(free_count, -1);
    builder.ins().store(
        MemFlags::trusted(),
        dec,
        slab,
        SlabLayout::HDR_FREE_COUNT,
    );

    // chunk_idx = (word_addr - bitmap_base) / 8 * 64 + bit_idx
    //           = ((word_addr - bitmap_base) << 3) + bit_idx
    let woff_bytes = builder.ins().isub(waddr, bitmap_base);
    let woff_bits = builder.ins().ishl_imm(woff_bytes, 3); // *8 → bit offset of word
    let chunk_idx = builder.ins().iadd(woff_bits, bit_idx);

    // chunk_addr = slab + payload_offset + chunk_idx * class_size
    let off_within = builder.ins().imul(chunk_idx, class_size_val);
    let payload_base = builder.ins().iadd(slab, payload_offset_val);
    let chunk_addr = builder.ins().iadd(payload_base, off_within);
    builder
        .ins()
        .jump(success_block, &[BlockArg::Value(chunk_addr)]);
}

/// Emit IR for `create_new_slab(class_idx_val) -> slab or 0`.
///
/// Overallocates `2 * slab_size`, aligns up, munmaps the unaligned
/// prefix/suffix, then initialises the header + bitmap.  Bitmap is filled
/// in a runtime loop so the spare bits in the last word (chunks past
/// `chunks_per_slab`) stay 0.
#[allow(clippy::too_many_arguments)]
fn emit_create_new_slab(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ty: Type,
    class_idx_val: Value,
    chunks_per_slab_val: Value,
    num_words_val: Value,
    slab_size_val: Value,
    mmap_ref: cranelift::codegen::ir::FuncRef,
    munmap_ref: cranelift::codegen::ir::FuncRef,
    success_block: cranelift::prelude::Block,
    fail_block: cranelift::prelude::Block,
) {
    let _ = ctx;

    // raw = mmap(2 * slab_size)
    let double_size = builder.ins().ishl_imm(slab_size_val, 1);
    let call = builder.ins().call(mmap_ref, &[double_size]);
    let raw = builder.inst_results(call)[0];

    // mmap returns MAP_FAILED = -1 on error.
    let neg_one = builder.ins().iconst(ty, -1i64);
    let bad = builder.ins().icmp(IntCC::Equal, raw, neg_one);
    let ok_block = builder.create_block();
    builder.ins().brif(bad, fail_block, &[], ok_block, &[]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);

    // aligned = (raw + slab_size - 1) & !(slab_size - 1)
    let slab_size_minus_one = builder.ins().iadd_imm(slab_size_val, -1);
    let raw_plus = builder.ins().iadd(raw, slab_size_minus_one);
    let inv_mask = builder.ins().bnot(slab_size_minus_one);
    let aligned = builder.ins().band(raw_plus, inv_mask);

    // prefix_len = aligned - raw
    let prefix_len = builder.ins().isub(aligned, raw);
    let prefix_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, prefix_len, 0);
    let prefix_unmap = builder.create_block();
    let after_prefix = builder.create_block();
    builder
        .ins()
        .brif(prefix_nonzero, prefix_unmap, &[], after_prefix, &[]);

    builder.switch_to_block(prefix_unmap);
    builder.seal_block(prefix_unmap);
    builder.ins().call(munmap_ref, &[raw, prefix_len]);
    builder.ins().jump(after_prefix, &[]);

    builder.switch_to_block(after_prefix);
    builder.seal_block(after_prefix);

    // suffix_start = aligned + slab_size
    // total raw = 2*slab_size starting at `raw`; aligned region occupies
    // [aligned, aligned + slab_size).  Suffix length = raw + 2*slab_size -
    // (aligned + slab_size) = slab_size - prefix_len.
    let suffix_start = builder.ins().iadd(aligned, slab_size_val);
    let suffix_len = builder.ins().isub(slab_size_val, prefix_len);
    let suffix_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, suffix_len, 0);
    let suffix_unmap = builder.create_block();
    let after_suffix = builder.create_block();
    builder
        .ins()
        .brif(suffix_nonzero, suffix_unmap, &[], after_suffix, &[]);

    builder.switch_to_block(suffix_unmap);
    builder.seal_block(suffix_unmap);
    builder.ins().call(munmap_ref, &[suffix_start, suffix_len]);
    builder.ins().jump(after_suffix, &[]);

    builder.switch_to_block(after_suffix);
    builder.seal_block(after_suffix);

    // Init header: class_id, free_count, next, prev all set.
    builder.ins().store(
        MemFlags::trusted(),
        class_idx_val,
        aligned,
        SlabLayout::HDR_CLASS_ID,
    );
    builder.ins().store(
        MemFlags::trusted(),
        chunks_per_slab_val,
        aligned,
        SlabLayout::HDR_FREE_COUNT,
    );
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().store(
        MemFlags::trusted(),
        zero,
        aligned,
        SlabLayout::HDR_NEXT_SLAB,
    );
    builder.ins().store(
        MemFlags::trusted(),
        zero,
        aligned,
        SlabLayout::HDR_PREV_SLAB,
    );

    // Bitmap fill loop: for word_idx in 0..num_words {
    //   remaining = chunks_per_slab - word_idx*64
    //   bits = min(64, remaining)
    //   word = bits == 64 ? -1 : (1 << bits) - 1
    //   *(bitmap_start + word_idx*8) = word
    // }
    let bitmap_start = builder
        .ins()
        .iadd_imm(aligned, SlabLayout::HDR_BITMAP_START as i64);

    let fill_hdr = builder.create_block();
    builder.append_block_param(fill_hdr, ty); // word_idx
    let fill_body = builder.create_block();
    let fill_done = builder.create_block();

    let zero2 = builder.ins().iconst(ty, 0);
    builder.ins().jump(fill_hdr, &[BlockArg::Value(zero2)]);

    builder.switch_to_block(fill_hdr);
    let widx = builder.block_params(fill_hdr)[0];
    let more = builder.ins().icmp(IntCC::UnsignedLessThan, widx, num_words_val);
    builder.ins().brif(more, fill_body, &[], fill_done, &[]);

    builder.switch_to_block(fill_body);
    builder.seal_block(fill_body);
    let widx_bits = builder.ins().imul_imm(widx, 64);
    let remaining = builder.ins().isub(chunks_per_slab_val, widx_bits);
    let sixty_four = builder.ins().iconst(ty, 64);
    let ge64 = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, remaining, sixty_four);
    // bits_in_word = min(64, remaining)
    let bits_in_word = builder.ins().select(ge64, sixty_four, remaining);
    // For full word (bits=64), `1 << 64` is UB; use a select to pick -1
    // explicitly when bits == 64.
    let one_v = builder.ins().iconst(ty, 1);
    let shifted = builder.ins().ishl(one_v, bits_in_word);
    let partial = builder.ins().iadd_imm(shifted, -1);
    let all_ones = builder.ins().iconst(ty, -1i64);
    let word_val = builder.ins().select(ge64, all_ones, partial);

    let word_off = builder.ins().imul_imm(widx, 8);
    let dst = builder.ins().iadd(bitmap_start, word_off);
    builder.ins().store(MemFlags::trusted(), word_val, dst, 0);

    let next_widx = builder.ins().iadd_imm(widx, 1);
    builder.ins().jump(fill_hdr, &[BlockArg::Value(next_widx)]);
    builder.seal_block(fill_hdr);

    builder.switch_to_block(fill_done);
    builder.seal_block(fill_done);
    builder.ins().jump(success_block, &[BlockArg::Value(aligned)]);
}

/// Build `rt_allocate_slab(class_idx: i64) -> i64`.  #150 phase 2.
///
/// Returns the address of a freshly-allocated chunk (NOT a fat-pointer).
/// The caller is responsible for laying out the 16-byte fat-ptr header
/// at chunk[0..16].  On OOM (mmap failed or class > supported), marks
/// the current actor dying and returns 0.
///
/// `class_idx` MUST be in `0..NUM_CLASSES` and must correspond to a
/// class with `chunks_per_slab(class_idx) > 0` (i.e. class 0..=11 for the
/// default 64 KiB slab size).  Classes 12+ trigger die_actor — phase 3
/// will add per-class oversized slabs.
pub fn define_allocate_slab(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let class_state_id = match ctx.module().get_name("slab_class_state") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "slab_class_state must be declared before rt_allocate_slab"
            ));
        }
    };
    let mmap_id = match ctx.module().get_name("rt_mmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_mmap must be declared before rt_allocate_slab")),
    };
    let munmap_id = match ctx.module().get_name("rt_munmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_munmap must be declared before rt_allocate_slab")),
    };
    let die_id = match ctx.module().get_name("rt_die_actor") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_die_actor must be declared before rt_allocate_slab")),
    };

    // ── Bake the per-class compile-time tables ─────────────────────────
    // `slab_chunks_per_class[class_idx] : i64`  — number of chunks per
    // slab for this class, 0 if the class needs an oversized slab.
    // `slab_payload_offset[class_idx]   : i64`  — payload-start offset
    // inside the slab (16-aligned, header + bitmap rounded up).
    // `slab_num_words[class_idx]        : i64`  — count of u64 bitmap
    // words to scan (= ceil(chunks_per_slab / 64)).
    let n = SlabLayout::NUM_CLASSES;
    let chunks_table: Vec<i64> = (0..n)
        .map(|i| SlabLayout::chunks_per_slab(i) as i64)
        .collect();
    let payload_table: Vec<i64> = (0..n).map(|i| payload_offset(i) as i64).collect();
    let words_table: Vec<i64> = (0..n).map(|i| num_words(i) as i64).collect();
    let slab_size_table: Vec<i64> = (0..n)
        .map(|i| SlabLayout::slab_size_for_class(i) as i64)
        .collect();

    let mut declare_const_table = |name: &str, vals: &[i64]| -> Result<cranelift::module::DataId> {
        let id = ctx
            .module_mut()
            .declare_data(name, Linkage::Local, false, false)?;
        let mut desc = DataDescription::new();
        desc.define(i64_le_bytes(vals).into_boxed_slice());
        ctx.module_mut().define_data(id, &desc)?;
        Ok(id)
    };
    let chunks_table_id = declare_const_table("slab_chunks_per_class", &chunks_table)?;
    let payload_table_id = declare_const_table("slab_payload_offset", &payload_table)?;
    let words_table_id = declare_const_table("slab_num_words", &words_table)?;
    let slab_size_table_id = declare_const_table("slab_size_per_class", &slab_size_table)?;

    // ── Function body ──────────────────────────────────────────────────
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let class_idx = builder.block_params(entry)[0];

    let cs_gv = ctx
        .module_mut()
        .declare_data_in_func(class_state_id, &mut builder.func);
    let chunks_gv = ctx
        .module_mut()
        .declare_data_in_func(chunks_table_id, &mut builder.func);
    let payload_gv = ctx
        .module_mut()
        .declare_data_in_func(payload_table_id, &mut builder.func);
    let words_gv = ctx
        .module_mut()
        .declare_data_in_func(words_table_id, &mut builder.func);
    let slab_size_gv = ctx
        .module_mut()
        .declare_data_in_func(slab_size_table_id, &mut builder.func);

    let mmap_ref = ctx
        .module_mut()
        .declare_func_in_func(mmap_id, &mut builder.func);
    let munmap_ref = ctx
        .module_mut()
        .declare_func_in_func(munmap_id, &mut builder.func);
    let die_ref = ctx
        .module_mut()
        .declare_func_in_func(die_id, &mut builder.func);

    let cs_base = builder.ins().global_value(ty, cs_gv);
    let chunks_base = builder.ins().global_value(ty, chunks_gv);
    let payload_base = builder.ins().global_value(ty, payload_gv);
    let words_base = builder.ins().global_value(ty, words_gv);
    let slab_size_base = builder.ins().global_value(ty, slab_size_gv);

    // cs_entry = cs_base + class_idx * 24
    let cs_off = builder
        .ins()
        .imul_imm(class_idx, SlabLayout::CLASS_STATE_SIZE as i64);
    let cs_entry = builder.ins().iadd(cs_base, cs_off);

    // class_idx * 8 → table offset
    let idx_x8 = builder.ins().imul_imm(class_idx, 8);
    let chunks_addr = builder.ins().iadd(chunks_base, idx_x8);
    let payload_addr = builder.ins().iadd(payload_base, idx_x8);
    let words_addr = builder.ins().iadd(words_base, idx_x8);
    let slab_size_addr = builder.ins().iadd(slab_size_base, idx_x8);

    let chunks_per_slab_val =
        builder.ins().load(ty, MemFlags::trusted(), chunks_addr, 0);
    let payload_offset_val =
        builder.ins().load(ty, MemFlags::trusted(), payload_addr, 0);
    let num_words_val =
        builder.ins().load(ty, MemFlags::trusted(), words_addr, 0);
    let slab_size_val =
        builder.ins().load(ty, MemFlags::trusted(), slab_size_addr, 0);

    // class_size = 1 << (class_idx + 4)
    let four = builder.ins().iconst(ty, 4);
    let shift = builder.ins().iadd(class_idx, four);
    let one_v = builder.ins().iconst(ty, 1);
    let class_size_val = builder.ins().ishl(one_v, shift);

    // If chunks_per_slab == 0, this class isn't supported by phase 2.
    // Die actor + return 0.
    let supported = builder
        .ins()
        .icmp_imm(IntCC::NotEqual, chunks_per_slab_val, 0);
    let supported_block = builder.create_block();
    let unsupported_block = builder.create_block();
    builder
        .ins()
        .brif(supported, supported_block, &[], unsupported_block, &[]);

    builder.switch_to_block(unsupported_block);
    builder.seal_block(unsupported_block);
    builder.ins().call(die_ref, &[]);
    let null = builder.ins().iconst(ty, 0);
    builder.ins().return_(&[null]);

    builder.switch_to_block(supported_block);
    builder.seal_block(supported_block);

    // ── Block plan ─────────────────────────────────────────────────────
    //
    //   supported → try_current
    //   try_current: if current_slab != 0 → try_alloc_from(current) else walk
    //   try_alloc_from_current:
    //     success → return_block(chunk)
    //     fail    → walk (start at slabs_head)
    //   walk_hdr(slab): if slab == 0 → need_new_slab else
    //                   if free_count(slab) > 0 → set_current + alloc_from(slab)
    //                   else → walk_hdr(next)
    //   need_new_slab:
    //     create_new_slab → success(new_slab) → link + alloc_from(new) → return
    //                     → fail (mmap failed) → die_actor + return 0
    //   alloc_from(slab) success → return_block(chunk)
    //                   fail (free_count became 0 mid-flight) → walk(next)

    let return_block = builder.create_block();
    builder.append_block_param(return_block, ty);
    let oom_block = builder.create_block();

    // try_current: load slabs_head, try fast path
    let current_slab = builder.ins().load(
        ty,
        MemFlags::trusted(),
        cs_entry,
        SlabLayout::CLASS_CURRENT_SLAB,
    );
    let has_current = builder.ins().icmp_imm(IntCC::NotEqual, current_slab, 0);
    let try_current_block = builder.create_block();
    let walk_start_block = builder.create_block();
    builder
        .ins()
        .brif(has_current, try_current_block, &[], walk_start_block, &[]);

    builder.switch_to_block(try_current_block);
    builder.seal_block(try_current_block);
    emit_try_alloc_from_slab(
        &mut builder,
        ty,
        current_slab,
        chunks_per_slab_val,
        payload_offset_val,
        class_size_val,
        num_words_val,
        return_block,
        walk_start_block,
    );

    // walk_start: load slabs_head into walk_hdr's param
    builder.switch_to_block(walk_start_block);
    builder.seal_block(walk_start_block);
    let head = builder.ins().load(
        ty,
        MemFlags::trusted(),
        cs_entry,
        SlabLayout::CLASS_SLABS_HEAD,
    );

    let walk_hdr = builder.create_block();
    builder.append_block_param(walk_hdr, ty); // slab
    builder.ins().jump(walk_hdr, &[BlockArg::Value(head)]);

    builder.switch_to_block(walk_hdr);
    let walk_slab = builder.block_params(walk_hdr)[0];
    let slab_nz = builder.ins().icmp_imm(IntCC::NotEqual, walk_slab, 0);
    let walk_inspect = builder.create_block();
    let need_new_block = builder.create_block();
    builder
        .ins()
        .brif(slab_nz, walk_inspect, &[], need_new_block, &[]);

    builder.switch_to_block(walk_inspect);
    builder.seal_block(walk_inspect);
    let walk_free = builder.ins().load(
        ty,
        MemFlags::trusted(),
        walk_slab,
        SlabLayout::HDR_FREE_COUNT,
    );
    let walk_has_free = builder.ins().icmp_imm(IntCC::NotEqual, walk_free, 0);
    let walk_alloc_block = builder.create_block();
    let walk_next_block = builder.create_block();
    builder
        .ins()
        .brif(walk_has_free, walk_alloc_block, &[], walk_next_block, &[]);

    // walk_next: slab = slab.next, loop.
    builder.switch_to_block(walk_next_block);
    builder.seal_block(walk_next_block);
    let next_slab = builder.ins().load(
        ty,
        MemFlags::trusted(),
        walk_slab,
        SlabLayout::HDR_NEXT_SLAB,
    );
    builder.ins().jump(walk_hdr, &[BlockArg::Value(next_slab)]);
    builder.seal_block(walk_hdr);

    // walk_alloc: update class_state.current_slab = slab, then alloc.  If
    // try_alloc fails (raced free_count → 0 — shouldn't happen single
    // threaded, but be defensive), fall through to walk_next of the same
    // slab to avoid infinite loops.  Single-threaded today, so the fail
    // edge points at need_new_block (definitely make progress).
    builder.switch_to_block(walk_alloc_block);
    builder.seal_block(walk_alloc_block);
    builder.ins().store(
        MemFlags::trusted(),
        walk_slab,
        cs_entry,
        SlabLayout::CLASS_CURRENT_SLAB,
    );
    emit_try_alloc_from_slab(
        &mut builder,
        ty,
        walk_slab,
        chunks_per_slab_val,
        payload_offset_val,
        class_size_val,
        num_words_val,
        return_block,
        need_new_block,
    );

    // need_new_block: create a fresh slab, link it, alloc from it.
    builder.switch_to_block(need_new_block);
    builder.seal_block(need_new_block);

    let new_slab_block = builder.create_block();
    builder.append_block_param(new_slab_block, ty); // slab
    emit_create_new_slab(
        &mut ctx,
        &mut builder,
        ty,
        class_idx,
        chunks_per_slab_val,
        num_words_val,
        slab_size_val,
        mmap_ref,
        munmap_ref,
        new_slab_block,
        oom_block,
    );

    // new_slab_block(slab): link as class head + current, alloc.
    builder.switch_to_block(new_slab_block);
    builder.seal_block(new_slab_block);
    let new_slab = builder.block_params(new_slab_block)[0];
    let old_head = builder.ins().load(
        ty,
        MemFlags::trusted(),
        cs_entry,
        SlabLayout::CLASS_SLABS_HEAD,
    );
    builder.ins().store(
        MemFlags::trusted(),
        old_head,
        new_slab,
        SlabLayout::HDR_NEXT_SLAB,
    );
    // If old_head != 0, update its prev pointer to new_slab.
    let oh_nz = builder.ins().icmp_imm(IntCC::NotEqual, old_head, 0);
    let link_prev_block = builder.create_block();
    let after_link_block = builder.create_block();
    builder
        .ins()
        .brif(oh_nz, link_prev_block, &[], after_link_block, &[]);

    builder.switch_to_block(link_prev_block);
    builder.seal_block(link_prev_block);
    builder.ins().store(
        MemFlags::trusted(),
        new_slab,
        old_head,
        SlabLayout::HDR_PREV_SLAB,
    );
    builder.ins().jump(after_link_block, &[]);

    builder.switch_to_block(after_link_block);
    builder.seal_block(after_link_block);
    builder.ins().store(
        MemFlags::trusted(),
        new_slab,
        cs_entry,
        SlabLayout::CLASS_SLABS_HEAD,
    );
    builder.ins().store(
        MemFlags::trusted(),
        new_slab,
        cs_entry,
        SlabLayout::CLASS_CURRENT_SLAB,
    );

    // Alloc from the brand-new slab.  Guaranteed to succeed (free_count
    // == chunks_per_slab > 0); on the off chance it doesn't, surface OOM
    // so we still make forward progress rather than spin.
    emit_try_alloc_from_slab(
        &mut builder,
        ty,
        new_slab,
        chunks_per_slab_val,
        payload_offset_val,
        class_size_val,
        num_words_val,
        return_block,
        oom_block,
    );

    // ── oom_block: mmap failed.  die_actor + return 0.
    builder.switch_to_block(oom_block);
    builder.seal_block(oom_block);
    builder.ins().call(die_ref, &[]);
    let null2 = builder.ins().iconst(ty, 0);
    builder.ins().return_(&[null2]);

    // ── return_block(chunk_addr): final exit.
    builder.switch_to_block(return_block);
    builder.seal_block(return_block);
    let chunk_addr = builder.block_params(return_block)[0];
    builder.ins().return_(&[chunk_addr]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_allocate_slab", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_free_slab(chunk_addr: i64)`.  #150 phase 3.
///
/// Returns a chunk to its owning slab's bitmap, then munmaps the
/// underlying slab if it becomes fully empty.  See
/// `docs/state/features/150_allocator_rewrite.md` for the algorithm.
///
/// Counterpart to `rt_allocate_slab` — phase 4 will splice them into
/// the legacy `rt_allocate` / `rt_free` ABI.
pub fn define_free_slab(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let class_state_id = match ctx.module().get_name("slab_class_state") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "slab_class_state must be declared before rt_free_slab"
            ));
        }
    };
    // Phase 2's tables are already baked; reuse them by name.
    let chunks_table_id = match ctx.module().get_name("slab_chunks_per_class") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "slab_chunks_per_class must be declared before rt_free_slab (phase 2 first)"
            ));
        }
    };
    let payload_table_id = match ctx.module().get_name("slab_payload_offset") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "slab_payload_offset must be declared before rt_free_slab (phase 2 first)"
            ));
        }
    };
    let slab_size_table_id = match ctx.module().get_name("slab_size_per_class") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            return Err(anyhow!(
                "slab_size_per_class must be declared before rt_free_slab (phase 5)"
            ));
        }
    };
    let munmap_id = match ctx.module().get_name("rt_munmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_munmap must be declared before rt_free_slab")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let chunk_addr = builder.block_params(entry)[0];

    let cs_gv = ctx
        .module_mut()
        .declare_data_in_func(class_state_id, &mut builder.func);
    let chunks_gv = ctx
        .module_mut()
        .declare_data_in_func(chunks_table_id, &mut builder.func);
    let payload_gv = ctx
        .module_mut()
        .declare_data_in_func(payload_table_id, &mut builder.func);
    let slab_size_gv = ctx
        .module_mut()
        .declare_data_in_func(slab_size_table_id, &mut builder.func);
    let munmap_ref = ctx
        .module_mut()
        .declare_func_in_func(munmap_id, &mut builder.func);

    let cs_base = builder.ins().global_value(ty, cs_gv);
    let chunks_base = builder.ins().global_value(ty, chunks_gv);
    let payload_base = builder.ins().global_value(ty, payload_gv);
    let slab_size_base = builder.ins().global_value(ty, slab_size_gv);

    // ── 1. Recover owning slab via address masking ─────────────────────
    // slab = chunk_addr & !(DEFAULT_SLAB_SIZE - 1).
    let slab_align_mask = builder
        .ins()
        .iconst(ty, !(SlabLayout::DEFAULT_SLAB_SIZE - 1));
    let slab = builder.ins().band(chunk_addr, slab_align_mask);

    // ── 2. Load class_id from header ───────────────────────────────────
    let class_id = builder
        .ins()
        .load(ty, MemFlags::trusted(), slab, SlabLayout::HDR_CLASS_ID);

    // ── 3. Compute chunk_idx ───────────────────────────────────────────
    // payload_start = slab + slab_payload_offset[class_id]
    // class_size_log2 = class_id + 4
    // chunk_idx = (chunk_addr - payload_start) >> class_size_log2
    let idx_x8 = builder.ins().imul_imm(class_id, 8);
    let payload_off_addr = builder.ins().iadd(payload_base, idx_x8);
    let payload_offset_val =
        builder.ins().load(ty, MemFlags::trusted(), payload_off_addr, 0);
    let payload_start = builder.ins().iadd(slab, payload_offset_val);
    let chunk_byte_off = builder.ins().isub(chunk_addr, payload_start);
    let class_shift = builder.ins().iadd_imm(class_id, 4);
    let chunk_idx = builder.ins().ushr(chunk_byte_off, class_shift);

    // ── 4. Set bitmap bit ──────────────────────────────────────────────
    // word_idx = chunk_idx >> 6;  bit_idx = chunk_idx & 63;
    // bitmap_word_addr = slab + HDR_BITMAP_START + word_idx*8.
    let word_idx = builder.ins().ushr_imm(chunk_idx, 6);
    let bit_idx = builder.ins().band_imm(chunk_idx, 63);
    let bitmap_base = builder
        .ins()
        .iadd_imm(slab, SlabLayout::HDR_BITMAP_START as i64);
    let word_off = builder.ins().imul_imm(word_idx, 8);
    let word_addr = builder.ins().iadd(bitmap_base, word_off);
    let word = builder.ins().load(ty, MemFlags::trusted(), word_addr, 0);
    let one_v = builder.ins().iconst(ty, 1);
    let bit_mask = builder.ins().ishl(one_v, bit_idx);
    let new_word = builder.ins().bor(word, bit_mask);
    builder.ins().store(MemFlags::trusted(), new_word, word_addr, 0);

    // ── 5. Increment free_count ────────────────────────────────────────
    let free_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        slab,
        SlabLayout::HDR_FREE_COUNT,
    );
    let new_free = builder.ins().iadd_imm(free_count, 1);
    builder.ins().store(
        MemFlags::trusted(),
        new_free,
        slab,
        SlabLayout::HDR_FREE_COUNT,
    );

    // ── 6. Skip munmap-on-empty: keep slabs in the class list for reuse.
    // Returning empty slabs to the OS sounds frugal but in a hot path
    // (lake-server scheduler allocates ~250 slab chunks per request)
    // it adds an mmap + 2 trim-munmap on the next alloc of the same
    // class — measured 4-5× slowdown vs reuse.  RSS is bounded by
    // peak working set (chunks_per_slab × num_classes_used), so we
    // pay memory once.  Direct-mmap huge_block path (class > 20)
    // still munmaps on free since each huge alloc is its own region.
    let chunks_addr = builder.ins().iadd(chunks_base, idx_x8);
    let chunks_per_slab_val =
        builder.ins().load(ty, MemFlags::trusted(), chunks_addr, 0);
    let _ = (new_free, chunks_per_slab_val);
    let is_empty = builder.ins().iconst(ty, 0);

    let reclaim_block = builder.create_block();
    let done_block = builder.create_block();
    builder
        .ins()
        .brif(is_empty, reclaim_block, &[], done_block, &[]);

    // ── reclaim: unlink + munmap ───────────────────────────────────────
    builder.switch_to_block(reclaim_block);
    builder.seal_block(reclaim_block);

    // cs_entry = cs_base + class_id * CLASS_STATE_SIZE
    let cs_off = builder
        .ins()
        .imul_imm(class_id, SlabLayout::CLASS_STATE_SIZE as i64);
    let cs_entry = builder.ins().iadd(cs_base, cs_off);

    let prev = builder
        .ins()
        .load(ty, MemFlags::trusted(), slab, SlabLayout::HDR_PREV_SLAB);
    let next = builder
        .ins()
        .load(ty, MemFlags::trusted(), slab, SlabLayout::HDR_NEXT_SLAB);

    // Fix prev's next pointer (or class list head).
    let prev_nz = builder.ins().icmp_imm(IntCC::NotEqual, prev, 0);
    let fix_prev_block = builder.create_block();
    let fix_head_block = builder.create_block();
    let after_prev_block = builder.create_block();
    builder
        .ins()
        .brif(prev_nz, fix_prev_block, &[], fix_head_block, &[]);

    builder.switch_to_block(fix_prev_block);
    builder.seal_block(fix_prev_block);
    builder.ins().store(
        MemFlags::trusted(),
        next,
        prev,
        SlabLayout::HDR_NEXT_SLAB,
    );
    builder.ins().jump(after_prev_block, &[]);

    builder.switch_to_block(fix_head_block);
    builder.seal_block(fix_head_block);
    builder.ins().store(
        MemFlags::trusted(),
        next,
        cs_entry,
        SlabLayout::CLASS_SLABS_HEAD,
    );
    builder.ins().jump(after_prev_block, &[]);

    builder.switch_to_block(after_prev_block);
    builder.seal_block(after_prev_block);

    // Fix next's prev pointer if next != 0.
    let next_nz = builder.ins().icmp_imm(IntCC::NotEqual, next, 0);
    let fix_next_block = builder.create_block();
    let after_next_block = builder.create_block();
    builder
        .ins()
        .brif(next_nz, fix_next_block, &[], after_next_block, &[]);

    builder.switch_to_block(fix_next_block);
    builder.seal_block(fix_next_block);
    builder.ins().store(
        MemFlags::trusted(),
        prev,
        next,
        SlabLayout::HDR_PREV_SLAB,
    );
    builder.ins().jump(after_next_block, &[]);

    builder.switch_to_block(after_next_block);
    builder.seal_block(after_next_block);

    // Clear current_slab cache if it pointed at us.  next-in-list might
    // be 0 — that's fine, next alloc walks slabs_head (also 0 here in
    // the single-slab case) and creates a fresh slab.
    let cur = builder.ins().load(
        ty,
        MemFlags::trusted(),
        cs_entry,
        SlabLayout::CLASS_CURRENT_SLAB,
    );
    let cur_is_us = builder.ins().icmp(IntCC::Equal, cur, slab);
    let clear_cur_block = builder.create_block();
    let after_clear_block = builder.create_block();
    builder
        .ins()
        .brif(cur_is_us, clear_cur_block, &[], after_clear_block, &[]);

    builder.switch_to_block(clear_cur_block);
    builder.seal_block(clear_cur_block);
    builder.ins().store(
        MemFlags::trusted(),
        next,
        cs_entry,
        SlabLayout::CLASS_CURRENT_SLAB,
    );
    builder.ins().jump(after_clear_block, &[]);

    builder.switch_to_block(after_clear_block);
    builder.seal_block(after_clear_block);

    // munmap(slab, slab_size_per_class[class_id]).  #150 phase 5 —
    // class >= 12 uses oversized slabs (slab_size = 2 * class_size),
    // class < 12 still uses DEFAULT_SLAB_SIZE.  Table lookup picks
    // the matching value baked at compile time.
    let slab_size_addr = builder.ins().iadd(slab_size_base, idx_x8);
    let slab_size_for_munmap =
        builder.ins().load(ty, MemFlags::trusted(), slab_size_addr, 0);
    builder.ins().call(munmap_ref, &[slab, slab_size_for_munmap]);
    builder.ins().jump(done_block, &[]);

    builder.switch_to_block(done_block);
    builder.seal_block(done_block);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_free_slab", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

#[cfg(test)]
mod slab_tests {
    use super::*;

    #[test]
    fn payload_offset_matches_chunks_per_slab() {
        // The payload offset should be reachable within DEFAULT_SLAB_SIZE for
        // every supported class.
        for i in 0..SlabLayout::NUM_CLASSES {
            let chunks = SlabLayout::chunks_per_slab(i);
            if chunks == 0 {
                continue;
            }
            let po = payload_offset(i);
            assert!(po >= SlabLayout::HDR_FIXED_BYTES as usize);
            assert_eq!(po % SlabLayout::CHUNK_ALIGN as usize, 0);
            assert!(
                po + chunks * SlabLayout::class_size(i)
                    <= SlabLayout::DEFAULT_SLAB_SIZE as usize,
                "class {i}: payload + chunks overflows slab",
            );
        }
    }

    #[test]
    fn num_words_covers_all_chunks() {
        for i in 0..SlabLayout::NUM_CLASSES {
            let chunks = SlabLayout::chunks_per_slab(i);
            let nw = num_words(i);
            assert!(nw * 64 >= chunks, "class {i}: words too few for chunks");
            // Tight: removing one word would leave bits uncovered.
            if nw > 0 {
                assert!((nw - 1) * 64 < chunks);
            }
        }
    }

    #[test]
    fn i64_le_bytes_round_trip() {
        let v = vec![0i64, 1, -1, 4062, 64 * 1024];
        let bytes = i64_le_bytes(&v);
        assert_eq!(bytes.len(), v.len() * 8);
        for (i, x) in v.iter().enumerate() {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            assert_eq!(i64::from_le_bytes(buf), *x);
        }
    }
}
