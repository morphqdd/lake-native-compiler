use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
        Type,
    },
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::FatPtrLayout};

/// Build `rt_allocate(size: i64) -> i64`.
///
/// Returns a **fat-pointer address** to the start of a header `{start, end}`
/// where `[start..end)` is the usable payload region of length ≥ `size`.
///
/// The allocator tries the free-list first (size-class bucket), falling back
/// to bump allocation in the 16 MiB heap region.  Buckets are powers of two
/// from 16 up to 4096 (9 buckets).  Allocations larger than 4096 bypass the
/// free list and bump-allocate the requested size directly — they cannot be
/// recycled (TODO).
pub fn define_allocate(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
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

    // ── Compute bucket index = ceil(log2(max(size, 16))) - 4 ────────────────
    // For sizes 16/32/64/128/256/512/1024/2048/4096 → buckets 0..8.
    // For oversized > 4096 → bucket > 8 → bypass the free list.
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

    // in_range = bucket_idx <= 8
    let max_bucket = builder.ins().iconst(ty, 8);
    let in_range = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bucket_idx, max_bucket);

    // ── Block layout ─────────────────────────────────────────────────────────
    // try_pop_block:    head = free_list[bucket]; if head != 0 jump pop_block
    // pop_block:        unlink head, jump merge_block(head)
    // bump_block:       bump-allocate (size = bucket_size when in_range, else user_size)
    // merge_block(fp):  return fp
    let try_pop_block = builder.create_block();
    let pop_block = builder.create_block();
    let bump_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, ty);

    // Decide try-pop vs bump up front based on `in_range`.
    builder
        .ins()
        .brif(in_range, try_pop_block, &[], bump_block, &[]);

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
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(head)]);

    // ── bump_block: classic bump allocation ─────────────────────────────────
    builder.switch_to_block(bump_block);
    builder.seal_block(bump_block);

    // Use bucket_size when in_range, user_size when oversized.
    let alloc_size = builder.ins().select(in_range, bucket_size, user_size);

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

    // Bounds check: trap if we'd exceed the heap.
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, end_addr, heap_end_addr);
    builder
        .ins()
        .trapz(in_bounds, TrapCode::HEAP_OUT_OF_BOUNDS);

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

    // ── merge_block: return the fat-pointer address ─────────────────────────
    builder.switch_to_block(merge_block);
    builder.seal_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_allocate", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_free(fat_ptr_addr: i64)`.
///
/// Reads the fat-pointer header to determine size, computes its bucket index,
/// and prepends the block to the free-list at that bucket.  The intrusive
/// next-pointer is stored at offset 0 of the payload.  Allocations whose
/// payload size > 4096 (bucket > 8) are leaked — we have no fall-back unmap.
///
/// All fat-pointers in Lake's runtime are heap-allocated via `rt_allocate`
/// (including the main process's resources after refactor); there are no
/// statically-defined fat-pointers reaching this function.  Future opt-in
/// `@static` machines will avoid `rt_allocate` entirely so they never reach
/// `rt_free` either.
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

    let max_bucket = builder.ins().iconst(ty, 8);
    let in_range = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bucket_idx, max_bucket);

    let push_block = builder.create_block();
    let leak_block = builder.create_block();
    builder
        .ins()
        .brif(in_range, push_block, &[], leak_block, &[]);

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

    // ── leak_block: oversized — drop on the floor (TODO) ─────────────────────
    builder.switch_to_block(leak_block);
    builder.seal_block(leak_block);
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

    builder.ins().store(MemFlags::new(), val, access_ptr, 0);
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
        builder
            .func
            .signature
            .returns
            .push(AbiParam::new(loaded_ty));

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

        let val = builder
            .ins()
            .load(loaded_ty, MemFlags::new(), access_ptr, 0);
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
