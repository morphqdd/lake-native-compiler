use anyhow::{Result, anyhow};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, TrapCode},
};

use crate::compiler::ctx::CompilerCtx;

const SYS_EXIT: i64 = 60; // Linux x86-64

/// Build and define `rt_exit(code: i64)` — calls `sys_exit` and never returns.
pub fn define_exit(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_exit")),
    };

    let mut builder_ctx = FunctionBuilderContext::default();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let code = builder.block_params(entry)[0];
    let sys_exit = builder.ins().iconst(ty, SYS_EXIT);
    let zero = builder.ins().iconst(ty, 0);
    builder
        .ins()
        .call(syscall_ref, &[sys_exit, code, zero, zero, zero, zero, zero]);
    // Unreachable; the trap tells Cranelift the block has a terminator.
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_exit", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
