use anyhow::{Result, bail};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
    },
};

use crate::compiler::ctx::CompilerCtx;

pub fn init_heap_memory_funcs(ctx: CompilerCtx) -> Result<CompilerCtx> {
    init_store(init_allocate(ctx)?)
}

fn init_allocate(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let user_size_val = builder.block_params(entry)[0];

    if let Some(FuncOrDataId::Data(heap_curr_id)) = ctx.module().get_name("heap_curr")
        && let Some(FuncOrDataId::Data(heap_end_id)) = ctx.module().get_name("heap_end")
    {
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

        let heap_header_size = builder.ins().iconst(ty, ty.bytes() as i64 * 2);
        let raw_user_ptr = builder.ins().iadd(heap_curr_addr, heap_header_size);

        let align = 16;

        let align_mask = builder.ins().iconst(ty, !(align - 1));
        let align = builder.ins().iconst(ty, align - 1);
        let raw_aligned_ptr = builder.ins().iadd(raw_user_ptr, align);
        let aligned_user_ptr = builder.ins().band(raw_aligned_ptr, align_mask);

        let end_addr = builder.ins().iadd(aligned_user_ptr, user_size_val);

        let cond = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, end_addr, heap_end_addr);
        builder.ins().trapz(cond, TrapCode::HEAP_OUT_OF_BOUNDS);

        builder
            .ins()
            .store(MemFlags::trusted(), aligned_user_ptr, heap_curr_addr, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), end_addr, heap_curr_addr, 8);

        builder
            .ins()
            .store(MemFlags::new(), end_addr, heap_curr_ptr, 0);

        builder.ins().return_(&[heap_curr_addr]);

        let allocate_sig = builder.func.signature.clone();
        let id =
            ctx.module_mut()
                .declare_function("rt_allocate", Linkage::Export, &allocate_sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;

        println!("allocate: {}", module_ctx.func);

        ctx.module_mut().clear_context(&mut module_ctx);

        return Ok(ctx);
    }

    Err(anyhow::anyhow!("Heap or mmap is not init"))
}

fn init_store(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fat_ptr, val, size, offset] = builder.block_params(entry)[0..4] else {
        bail!("Not 4 params")
    };

    let user_data_base_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 0);
    let user_data_offeset_ptr = builder.ins().iadd(user_data_base_ptr, offset);
    let ptr_to_end_of_val = builder.ins().iadd(user_data_offeset_ptr, size);
    let user_data_end_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 8);

    let cond = builder.ins().icmp(
        IntCC::UnsignedLessThan,
        ptr_to_end_of_val,
        user_data_end_ptr,
    );

    builder.ins().trapz(cond, TrapCode::unwrap_user(32));

    builder
        .ins()
        .store(MemFlags::new(), val, user_data_offeset_ptr, 0);

    builder.ins().return_(&[]);

    let store_sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_store", Linkage::Export, &store_sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;

    println!("store_func: {}", module_ctx.func);

    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
