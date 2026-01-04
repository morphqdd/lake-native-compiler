use anyhow::Result;
use cranelift::{
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder},
};

use crate::compiler::ctx::CompilerCtx;

const SYS_MMAP: i64 = 9;

const PROT_READ: i64 = 0x1;
const PROT_WRITE: i64 = 0x2;

const MAP_PRIVATE: i64 = 0x02;
const MAP_ANONYMOUS: i64 = 0x20;

pub fn init_mmap_func(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    // let mut mmap_sig = ctx.module().make_signature();
    // mmap_sig.params.push(AbiParam::new(ty));
    // mmap_sig.returns.push(AbiParam::new(ty));

    if let Some(func_id) = ctx.module().get_name("rt_syscall")
        && let FuncOrDataId::Func(func_id) = func_id
    {
        let mut builder_ctx = FunctionBuilderContext::default();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

        let syscall_ref = ctx
            .module_mut()
            .declare_func_in_func(func_id, &mut builder.func);

        builder.func.signature.params.push(AbiParam::new(ty));
        builder.func.signature.returns.push(AbiParam::new(ty));

        let entry_block = builder.create_block();
        builder.append_block_param(entry_block, ty);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let syscall_ty = builder.ins().iconst(ty, SYS_MMAP);
        let addr = builder.ins().iconst(ty, 0);
        let size = builder.block_params(entry_block)[0];
        let prot = builder.ins().iconst(ty, PROT_READ | PROT_WRITE);
        let flags = builder.ins().iconst(ty, MAP_ANONYMOUS | MAP_PRIVATE);
        let fd = builder.ins().iconst(ty, -1);
        let offset = builder.ins().iconst(ty, 0);

        let call = builder.ins().call(
            syscall_ref,
            &[syscall_ty, addr, size, prot, flags, fd, offset],
        );

        let ret = builder.inst_results(call)[0];
        builder.ins().return_(&[ret]);

        let mmap_sig = builder.func.signature.clone();
        let id = ctx
            .module_mut()
            .declare_function("rt_mmap", Linkage::Export, &mmap_sig)?;

        ctx.module_mut().define_function(id, &mut module_ctx)?;

        let mut data_description = DataDescription::new();
        data_description.define_zeroinit(ty.bytes() as usize);
        let heap_base_id =
            ctx.module_mut()
                .declare_data("heap_base", Linkage::Export, true, false)?;
        ctx.module_mut()
            .define_data(heap_base_id, &data_description)?;
        let heap_curr_id =
            ctx.module_mut()
                .declare_data("heap_curr", Linkage::Export, true, false)?;
        ctx.module_mut()
            .define_data(heap_curr_id, &data_description)?;
        let heap_end_id =
            ctx.module_mut()
                .declare_data("heap_end", Linkage::Export, true, false)?;
        ctx.module_mut()
            .define_data(heap_end_id, &data_description)?;
        println!("mmap: {}", module_ctx.func);

        ctx.module_mut().clear_context(&mut module_ctx);

        return Ok(ctx);
    }

    Err(anyhow::anyhow!("syscall wrapper is not init"))
}
