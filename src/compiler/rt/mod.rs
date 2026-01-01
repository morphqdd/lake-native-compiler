use anyhow::{Result, bail};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, TrapCode},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::{
        alloc::init_mmap_func,
        rt_utils::{exit::init_exit_func, init_syscall_wrapper},
    },
};

mod alloc;
mod process;
mod rt_utils;

pub struct Runtime {}

impl Default for Runtime {
    fn default() -> Self {
        Self {}
    }
}

impl Runtime {
    fn init(&self, ctx: CompilerCtx) -> Result<CompilerCtx> {
        init_mmap_func(init_exit_func(init_syscall_wrapper(ctx)?)?)
    }
    pub fn build(self, ctx: CompilerCtx) -> Result<CompilerCtx> {
        let mut ctx = self.init(ctx)?;

        let pointer_type = ctx.module().target_config().pointer_type();
        let mut builder_ctx = FunctionBuilderContext::default();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

        builder
            .func
            .signature
            .returns
            .push(AbiParam::new(pointer_type));

        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let Some(FuncOrDataId::Func(exit_id)) = ctx.module().get_name("exit") else {
            bail!("RT func 'exit' is not define")
        };

        let exit_ref = ctx
            .module_mut()
            .declare_func_in_func(exit_id, &mut builder.func);

        let zero = builder.ins().iconst(pointer_type, 10);

        builder.ins().call(exit_ref, &[zero]);
        builder.ins().trap(TrapCode::user(0xDE).unwrap());

        let rt_sig = builder.func.signature.clone();

        let id = ctx
            .module_mut()
            .declare_function("_start", Linkage::Export, &rt_sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;

        println!("rt: {}", module_ctx.func);

        ctx.module_mut().clear_context(&mut module_ctx);
        Ok(ctx)
    }
}
