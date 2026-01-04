use anyhow::{Result, bail};
use cranelift::{
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, EntityRef, FunctionBuilder, FunctionBuilderContext, Imm64, InstBuilder, MemFlags,
        TrapCode, Type, Value,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::{
        alloc::heap_memory_operations::init_heap_memory_funcs,
        alloc::mmap::init_mmap_func,
        rt_utils::{exit::init_exit_func, init_syscall_wrapper},
    },
};

mod alloc;
mod process;
mod rt_utils;
mod rw;

const HEAP_SIZE: i64 = 16 * 1024 * 1024;

pub struct Runtime {}

impl Default for Runtime {
    fn default() -> Self {
        Self {}
    }
}

impl Runtime {
    fn init(&self, ctx: CompilerCtx) -> Result<CompilerCtx> {
        init_heap_memory_funcs(init_mmap_func(init_exit_func(init_syscall_wrapper(ctx)?)?)?)
    }
    pub fn build(self, ctx: CompilerCtx) -> Result<CompilerCtx> {
        let mut ctx = self.init(ctx)?;

        let pointer_type = ctx.module().target_config().pointer_type();
        let mut builder_ctx = FunctionBuilderContext::default();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        if let Some(FuncOrDataId::Func(mmap_id)) = ctx.module().get_name("rt_mmap")
            && let Some(FuncOrDataId::Data(heap_base_id)) = ctx.module().get_name("heap_base")
            && let Some(FuncOrDataId::Data(heap_curr_id)) = ctx.module().get_name("heap_curr")
            && let Some(FuncOrDataId::Data(heap_end_id)) = ctx.module().get_name("heap_end")
        {
            let mmap_ref = ctx
                .module_mut()
                .declare_func_in_func(mmap_id, &mut builder.func);
            let heap_size = builder.ins().iconst(pointer_type, HEAP_SIZE);

            let mmap_call = builder.ins().call(mmap_ref, &[heap_size]);

            let heap_base_addr = builder.inst_results(mmap_call)[0];
            let heap_end_addr = builder.ins().iadd(heap_base_addr, heap_size);

            let heap_base_gv = ctx
                .module_mut()
                .declare_data_in_func(heap_base_id, &mut builder.func);

            let heap_curr_gv = ctx
                .module_mut()
                .declare_data_in_func(heap_curr_id, &mut builder.func);

            let heap_end_gv = ctx
                .module_mut()
                .declare_data_in_func(heap_end_id, &mut builder.func);

            let heap_base_ptr = builder.ins().global_value(pointer_type, heap_base_gv);
            let heap_curr_ptr = builder.ins().global_value(pointer_type, heap_curr_gv);
            let heap_end_ptr = builder.ins().global_value(pointer_type, heap_end_gv);

            builder
                .ins()
                .store(MemFlags::new(), heap_base_addr, heap_base_ptr, 0);
            builder
                .ins()
                .store(MemFlags::new(), heap_base_addr, heap_curr_ptr, 0);
            builder
                .ins()
                .store(MemFlags::new(), heap_end_addr, heap_end_ptr, 0);
        }

        let Some(FuncOrDataId::Func(allocate_id)) = ctx.module().get_name("rt_allocate") else {
            bail!("RT func 'allocate' is not define")
        };

        let allocate_ref = ctx
            .module_mut()
            .declare_func_in_func(allocate_id, &mut builder.func);

        let buf_size = builder.ins().iconst(pointer_type, 32);
        let allocate_call = builder.ins().call(allocate_ref, &[buf_size]);

        let ptr = builder.inst_results(allocate_call)[0];

        let Some(FuncOrDataId::Func(store_id)) = ctx.module().get_name("rt_store") else {
            bail!("RT func 'store' is not define")
        };

        let store_ref = ctx
            .module_mut()
            .declare_func_in_func(store_id, &mut builder.func);

        let u8_type = Type::int(8).unwrap();
        let h = builder.ins().iconst(pointer_type, 'H' as u8 as i64);
        let e = builder.ins().iconst(pointer_type, 'e' as u8 as i64);
        let l = builder.ins().iconst(pointer_type, 'l' as u8 as i64);
        let o = builder.ins().iconst(pointer_type, 'o' as u8 as i64);

        let u8_type_size = builder.ins().iconst(pointer_type, 1);
        let i0 = builder.ins().iconst(pointer_type, 0);
        let i1 = builder.ins().iconst(pointer_type, 1);
        let i2 = builder.ins().iconst(pointer_type, 2);
        let i3 = builder.ins().iconst(pointer_type, 3);
        let i4 = builder.ins().iconst(pointer_type, 4);

        builder.ins().call(store_ref, &[ptr, h, u8_type_size, i0]);
        builder.ins().call(store_ref, &[ptr, e, u8_type_size, i1]);
        builder.ins().call(store_ref, &[ptr, l, u8_type_size, i2]);
        builder.ins().call(store_ref, &[ptr, l, u8_type_size, i3]);
        builder.ins().call(store_ref, &[ptr, o, u8_type_size, i4]);

        let Some(FuncOrDataId::Func(exit_id)) = ctx.module().get_name("rt_exit") else {
            bail!("RT func 'exit' is not define")
        };

        let exit_ref = ctx
            .module_mut()
            .declare_func_in_func(exit_id, &mut builder.func);

        let zero = builder.inst_results(allocate_call)[0];
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
