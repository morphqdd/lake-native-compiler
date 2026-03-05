use anyhow::{Result, anyhow};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
        Type,
    },
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::FatPtrLayout};

/// Build `rt_allocate(size: i64) -> i64` (returns fat-ptr address).
pub fn define_allocate(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let (heap_curr_id, heap_end_id) = match (
        ctx.module().get_name("heap_curr"),
        ctx.module().get_name("heap_end"),
    ) {
        (Some(FuncOrDataId::Data(c)), Some(FuncOrDataId::Data(e))) => (c, e),
        _ => return Err(anyhow!("Heap globals must be declared before rt_allocate")),
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

    let heap_curr_ptr = builder.ins().global_value(ty, heap_curr_gv);
    let heap_end_ptr = builder.ins().global_value(ty, heap_end_gv);

    let heap_curr_addr = builder.ins().load(ty, MemFlags::new(), heap_curr_ptr, 0);
    let heap_end_addr = builder.ins().load(ty, MemFlags::new(), heap_end_ptr, 0);

    // Skip the 16-byte fat-pointer header to get the start of user data.
    let header = builder.ins().iconst(ty, FatPtrLayout::SIZE as i64);
    let raw_user_ptr = builder.ins().iadd(heap_curr_addr, header);

    // Align to 16 bytes.
    let align_mask = builder.ins().iconst(ty, !(16i64 - 1));
    let align_add = builder.ins().iconst(ty, 16 - 1);
    let unaligned = builder.ins().iadd(raw_user_ptr, align_add);
    let aligned_user_ptr = builder.ins().band(unaligned, align_mask);

    let end_addr = builder.ins().iadd(aligned_user_ptr, user_size);

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
        .store(MemFlags::new(), end_addr, heap_curr_ptr, 0);

    builder.ins().return_(&[heap_curr_addr]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_allocate", Linkage::Export, &sig)?;
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
