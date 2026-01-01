use anyhow::Result;
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, TrapCode},
};

use crate::compiler::ctx::CompilerCtx;

const SYS_EXIT: i64 = 60;

pub fn init_exit_func(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    if let Some(func_id) = ctx.module().get_name("syscall")
        && let FuncOrDataId::Func(func_id) = func_id
    {
        let mut builder_ctx = FunctionBuilderContext::default();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

        let syscall_ref = ctx
            .module_mut()
            .declare_func_in_func(func_id, &mut builder.func);

        builder.func.signature.params.push(AbiParam::new(ty));

        let entry = builder.create_block();
        builder.append_block_param(entry, ty); // code
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let sys_exit = builder.ins().iconst(ty, SYS_EXIT);
        let code = builder.block_params(entry)[0];
        let zero = builder.ins().iconst(ty, 0);
        builder
            .ins()
            .call(syscall_ref, &[sys_exit, code, zero, zero, zero, zero, zero]);

        builder.ins().trap(TrapCode::user(0xDE).unwrap());

        let exit_sig = builder.func.signature.clone();
        let id = ctx
            .module_mut()
            .declare_function("exit", Linkage::Export, &exit_sig)?;

        ctx.module_mut().define_function(id, &mut module_ctx)?;

        println!("exit: {}", module_ctx.func);

        ctx.module_mut().clear_context(&mut module_ctx);

        return Ok(ctx);
    }

    Err(anyhow::anyhow!("syscall wrapper is not init"))
}
