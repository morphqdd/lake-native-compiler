use anyhow::{Result, bail};
use cranelift::{
    module::{FuncOrDataId, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext},
};

use crate::compiler::ctx::CompilerCtx;

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

    let [fd, ptr, size] = builder.block_params(entry)[0..3] else {
        bail!("Need 3 params");
    };

    Ok(ctx)
}
