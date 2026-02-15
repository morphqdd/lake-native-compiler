use anyhow::Result;
use cranelift::{
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Value},
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::ExecCtxLayout};

pub struct ProcessCtxLayout;

impl ProcessCtxLayout {
    pub const SIZE: i32 = 16;
    pub const FUNC_PTR: i32 = 0;
    pub const EXEC_CTX: i32 = 8;

    pub fn init_ctx(
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
        name: &str,
        exec_ctx: Value,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_funcs = ctx.rt_funcs().clone();
        let process_func = ctx.get_func(builder, name)?;
        let allocate_ref = rt_funcs.allocate_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);

        let func_addr = builder.ins().func_addr(ptr_ty, process_func);

        let process_ctx_size = builder.ins().iconst(ptr_ty, Self::SIZE as i64);
        let call_alloc = builder.ins().call(allocate_ref, &[process_ctx_size]);
        let process_ctx_ptr = builder.inst_results(call_alloc)[0];

        let func_ptr_offset = builder.ins().iconst(ptr_ty, Self::FUNC_PTR as i64);

        builder.ins().call(
            store_ref,
            &[process_ctx_ptr, func_addr, ptr_size, func_ptr_offset],
        );

        let exec_ctx_offset = builder.ins().iconst(ptr_ty, Self::EXEC_CTX as i64);
        builder.ins().call(
            store_ref,
            &[process_ctx_ptr, exec_ctx, ptr_size, exec_ctx_offset],
        );

        Ok(process_ctx_ptr)
    }

    pub fn get_func_addr(
        process_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let offset = builder.ins().iconst(ptr_ty, Self::FUNC_PTR as i64);
        let call_load_func_addr = builder
            .ins()
            .call(load_func_ref, &[process_ctx_ptr, offset]);
        let func_addr = builder.inst_results(call_load_func_addr)[0];
        Ok(func_addr)
    }

    pub fn get_exec_ctx(
        process_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let offset = builder.ins().iconst(ptr_ty, Self::EXEC_CTX as i64);
        let call_load_exec_ctx = builder
            .ins()
            .call(load_func_ref, &[process_ctx_ptr, offset]);
        let exec_ctx = builder.inst_results(call_load_exec_ctx)[0];
        Ok(exec_ctx)
    }
}
