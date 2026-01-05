use anyhow::{Result, bail};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
    },
};

use crate::compiler::ctx::CompilerCtx;

const SYSCALL_WRITE: i64 = 1;

pub fn init_write(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let mut module_ctx = ctx.module().make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));

    let Some(FuncOrDataId::Func(syscall_id)) = ctx.module().get_name("rt_syscall") else {
        bail!("Syscall is not init");
    };

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fd, fat_ptr, size] = builder.block_params(entry)[0..3] else {
        bail!("Need 3 params");
    };

    let syscall_ty = builder.ins().iconst(ty, SYSCALL_WRITE);
    let zero = builder.ins().iconst(ty, 0);
    let user_data_base_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 0);
    let ptr_to_end_of_val = builder.ins().iadd(user_data_base_ptr, size);
    let user_data_end_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 8);

    let cond = builder.ins().icmp(
        IntCC::UnsignedLessThan,
        ptr_to_end_of_val,
        user_data_end_ptr,
    );

    builder.ins().trapz(cond, TrapCode::unwrap_user(32));

    builder.ins().call(
        syscall_ref,
        &[syscall_ty, fd, user_data_base_ptr, size, zero, zero, zero],
    );

    builder.ins().return_(&[]);

    let write_sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_write", Linkage::Export, &write_sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;

    println!("rt_write_func: {}", module_ctx.func);

    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

pub fn init_write_static(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let mut module_ctx = ctx.module().make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));

    let Some(FuncOrDataId::Func(syscall_id)) = ctx.module().get_name("rt_syscall") else {
        bail!("Syscall is not init");
    };

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fd, ptr, size] = builder.block_params(entry)[0..3] else {
        bail!("Need 3 params");
    };

    let syscall_ty = builder.ins().iconst(ty, SYSCALL_WRITE);
    let zero = builder.ins().iconst(ty, 0);

    builder
        .ins()
        .call(syscall_ref, &[syscall_ty, fd, ptr, size, zero, zero, zero]);

    builder.ins().return_(&[]);

    let write_sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_write_static", Linkage::Export, &write_sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;

    println!("rt_write_func: {}", module_ctx.func);

    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
