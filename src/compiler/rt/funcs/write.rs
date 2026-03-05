use anyhow::{Result, anyhow};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, TrapCode,
    },
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::FatPtrLayout};

const SYSCALL_WRITE: i64 = 1; // Linux x86-64

/// Build `rt_write(fd, fat_ptr, size)` — bounds-checked write syscall.
pub fn define_write(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let syscall_id = get_syscall_id(&ctx)?;

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    for _ in 0..3 {
        builder.func.signature.params.push(AbiParam::new(ty));
    }

    let entry = builder.create_block();
    for _ in 0..3 {
        builder.append_block_param(entry, ty);
    }
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fd, fat_ptr, size] = builder.block_params(entry)[0..3] else {
        unreachable!()
    };

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    // Bounds check: start + size <= end.
    let start = FatPtrLayout::load_start(&mut builder, ty, fat_ptr);
    let end = FatPtrLayout::load_end(&mut builder, ty, fat_ptr);
    let access_end = builder.ins().iadd(start, size);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
    builder
        .ins()
        .trapz(in_bounds, TrapCode::unwrap_user(32));

    let syscall_nr = builder.ins().iconst(ty, SYSCALL_WRITE);
    let zero = builder.ins().iconst(ty, 0);
    builder
        .ins()
        .call(syscall_ref, &[syscall_nr, fd, start, size, zero, zero, zero]);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_write", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_write_static(fd, ptr, size)` — unchecked write (for static buffers).
pub fn define_write_static(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let syscall_id = get_syscall_id(&ctx)?;

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    for _ in 0..3 {
        builder.func.signature.params.push(AbiParam::new(ty));
    }

    let entry = builder.create_block();
    for _ in 0..3 {
        builder.append_block_param(entry, ty);
    }
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fd, ptr, size] = builder.block_params(entry)[0..3] else {
        unreachable!()
    };

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let syscall_nr = builder.ins().iconst(ty, SYSCALL_WRITE);
    let zero = builder.ins().iconst(ty, 0);
    builder
        .ins()
        .call(syscall_ref, &[syscall_nr, fd, ptr, size, zero, zero, zero]);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_write_static", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

fn get_syscall_id(ctx: &CompilerCtx) -> Result<cranelift::module::FuncId> {
    match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => Ok(id),
        _ => Err(anyhow!("rt_syscall must be declared before write functions")),
    }
}
