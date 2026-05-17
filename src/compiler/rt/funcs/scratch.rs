use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{ExecCtxLayout, FatPtrLayout},
};

/// Build `rt_scratch_buf(exec_ctx_ptr: i64, size: i64) -> i64`.
///
/// Returns a fat-pointer address into one of 8 per-actor scratch slots
/// in `ExecCtx.SCRATCH_RING`.  Slot index is `size mod 8` (deterministic
/// reuse for repeat-same-size).  If the cached slot is large enough,
/// reuse its storage and hand back a fresh fat-pointer header pointing
/// at it; otherwise call `rt_allocate_raw(size)`, cache the data
/// pointer + size in the slot, and return the freshly-allocated fat
/// pointer.
///
/// User code does NOT free — slot is overwritten on next
/// `rt_scratch_buf(same_slot)` call, or abandoned at STOP_DONE.
///
/// See docs/state/features/082_scratch_buf_pool.md for context.
pub fn define_scratch_buf(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_allocate_raw must be declared before rt_scratch_buf"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    // exec_ctx_ptr, size → fat_ptr_addr
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let exec_ctx_ptr = builder.block_params(entry)[0];
    let size = builder.block_params(entry)[1];

    let alloc_raw_ref = ctx
        .module_mut()
        .declare_func_in_func(alloc_raw_id, &mut builder.func);

    // slot_idx = size & (SCRATCH_SLOTS-1)  — SCRATCH_SLOTS is a power of 2.
    let mask = builder
        .ins()
        .iconst(ty, (ExecCtxLayout::SCRATCH_SLOTS as i64) - 1);
    let slot_idx = builder.ins().band(size, mask);
    // slot_addr = exec_ctx_ptr + SCRATCH_RING + slot_idx * SCRATCH_SLOT_SIZE
    let slot_byte_off = builder
        .ins()
        .imul_imm(slot_idx, ExecCtxLayout::SCRATCH_SLOT_SIZE as i64);
    let ring_base = builder
        .ins()
        .iadd_imm(exec_ctx_ptr, ExecCtxLayout::SCRATCH_RING as i64);
    let slot_addr = builder.ins().iadd(ring_base, slot_byte_off);

    let cached_ptr = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 0);
    let cached_cap = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 8);

    // reuse iff cached_ptr != 0 && cached_cap >= size
    let ptr_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, cached_ptr, 0);
    let cap_ok = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, cached_cap, size);
    let reuse = builder.ins().band(ptr_nonzero, cap_ok);

    let reuse_block = builder.create_block();
    let alloc_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, ty);

    builder
        .ins()
        .brif(reuse, reuse_block, &[], alloc_block, &[]);

    // ── reuse_block: build a fresh fat-ptr header over the cached storage.
    // old slot ptr abandoned in arena; see #75/#119.
    builder.switch_to_block(reuse_block);
    builder.seal_block(reuse_block);
    let fat_hdr_size = builder.ins().iconst(ty, FatPtrLayout::SIZE as i64);
    let call_hdr = builder.ins().call(alloc_raw_ref, &[fat_hdr_size]);
    let hdr_fat = builder.inst_results(call_hdr)[0];
    // hdr_fat is a fat-ptr whose payload is 16 bytes — we won't actually use
    // that payload as data; we hijack the just-allocated fat-ptr struct itself
    // and rewrite its {start, end} to span the cached buffer.
    let cached_end = builder.ins().iadd(cached_ptr, size);
    builder
        .ins()
        .store(MemFlags::trusted(), cached_ptr, hdr_fat, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), cached_end, hdr_fat, 8);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(hdr_fat)]);

    // ── alloc_block: fresh allocation, cache its data ptr + size.
    builder.switch_to_block(alloc_block);
    builder.seal_block(alloc_block);
    let call_new = builder.ins().call(alloc_raw_ref, &[size]);
    let new_fat = builder.inst_results(call_new)[0];
    let new_start = builder.ins().load(ty, MemFlags::trusted(), new_fat, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), new_start, slot_addr, 0);
    builder.ins().store(MemFlags::trusted(), size, slot_addr, 8);
    builder
        .ins()
        .jump(merge_block, &[BlockArg::Value(new_fat)]);

    // ── merge_block: return fat-ptr.
    builder.switch_to_block(merge_block);
    builder.seal_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_scratch_buf", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}
