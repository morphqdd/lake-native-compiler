use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, EntityRef, FunctionBuilder, FunctionBuilderContext, Imm64, InstBuilder, IntCC,
        MemFlags, TrapCode, Type, Value,
    },
};

use crate::compiler::{
    BLOCK_ID, BRANCH_ID, CTX_SIZE, JUMP_ARGS, VARIABLES,
    ctx::CompilerCtx,
    rt::{
        alloc::{heap_memory_operations::init_heap_memory_funcs, mmap::init_mmap_func},
        rt_utils::{exit::init_exit_func, init_syscall_wrapper},
        rw::init_rw,
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
    pub fn init(&self, ctx: CompilerCtx) -> Result<CompilerCtx> {
        init_rw(init_heap_memory_funcs(init_mmap_func(init_exit_func(
            init_syscall_wrapper(ctx)?,
        )?)?)?)
    }
    pub fn build(self, mut ctx: CompilerCtx) -> Result<CompilerCtx> {
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

        let main_ref = ctx.get_func(&mut builder, "main")?;

        let branch_id = ctx.lookup_param_count("main", 0).unwrap();
        let branch_vars_count = ctx.lookup_vars_count(branch_id).unwrap();

        println!("branch_vars_count: {branch_vars_count}");

        let main_vars_id =
            ctx.module_mut()
                .declare_data("main_vars", Linkage::Export, true, false)?;
        let mut main_vars_data = DataDescription::new();
        main_vars_data.define_zeroinit(branch_vars_count * 8);
        ctx.module_mut()
            .define_data(main_vars_id, &main_vars_data)?;
        let main_vars_gv = ctx
            .module_mut()
            .declare_data_in_func(main_vars_id, &mut builder.func);
        let main_vars_ptr = builder.ins().global_value(pointer_type, main_vars_gv);

        let main_vars_fat_ptr_id =
            ctx.module_mut()
                .declare_data("main_vars_fat_ptr", Linkage::Export, true, false)?;
        let mut main_vars_fat_ptr_data = DataDescription::new();
        main_vars_fat_ptr_data.define_zeroinit(16);
        ctx.module_mut()
            .define_data(main_vars_fat_ptr_id, &main_vars_fat_ptr_data)?;
        let main_vars_fat_ptr_gv = ctx
            .module_mut()
            .declare_data_in_func(main_vars_fat_ptr_id, &mut builder.func);
        let main_vars_fat_ptr = builder
            .ins()
            .global_value(pointer_type, main_vars_fat_ptr_gv);

        let end_ptr = builder
            .ins()
            .iadd_imm(main_vars_ptr, branch_vars_count as i64 * 8);
        builder
            .ins()
            .store(MemFlags::new(), main_vars_ptr, main_vars_fat_ptr, 0);
        builder
            .ins()
            .store(MemFlags::new(), end_ptr, main_vars_fat_ptr, 8);

        let main_args_id =
            ctx.module_mut()
                .declare_data("main_args", Linkage::Export, true, false)?;
        let mut main_args_data = DataDescription::new();
        main_args_data.define_zeroinit(256 * 8);
        ctx.module_mut()
            .define_data(main_args_id, &main_args_data)?;
        let main_args_gv = ctx
            .module_mut()
            .declare_data_in_func(main_args_id, &mut builder.func);
        let main_args_ptr = builder.ins().global_value(pointer_type, main_args_gv);
        let main_args_fat_ptr_id =
            ctx.module_mut()
                .declare_data("main_args_fat_ptr", Linkage::Export, true, false)?;
        let mut main_args_fat_ptr_data = DataDescription::new();
        main_args_fat_ptr_data.define_zeroinit(16);
        ctx.module_mut()
            .define_data(main_args_fat_ptr_id, &main_args_fat_ptr_data)?;
        let main_args_fat_ptr_gv = ctx
            .module_mut()
            .declare_data_in_func(main_args_fat_ptr_id, &mut builder.func);
        let main_args_fat_ptr = builder
            .ins()
            .global_value(pointer_type, main_args_fat_ptr_gv);

        let end_ptr = builder.ins().iadd_imm(main_args_ptr, 256 * 8);
        builder
            .ins()
            .store(MemFlags::new(), main_args_ptr, main_args_fat_ptr, 0);
        builder
            .ins()
            .store(MemFlags::new(), end_ptr, main_args_fat_ptr, 8);

        let branch_id_val = builder.ins().iconst(pointer_type, branch_id as i64);

        let main_ctx_id =
            ctx.module_mut()
                .declare_data("main_ctx", Linkage::Export, true, false)?;

        let mut main_ctx_data = DataDescription::new();

        main_ctx_data.define_zeroinit(CTX_SIZE as usize);

        ctx.module_mut().define_data(main_ctx_id, &main_ctx_data)?;

        let main_ctx_fat_ptr =
            ctx.module_mut()
                .declare_data("main_ctx_fat_ptr", Linkage::Export, true, false)?;
        let mut main_ctx_fat_ptr_data = DataDescription::new();
        main_ctx_fat_ptr_data.define_zeroinit(16);
        ctx.module_mut()
            .define_data(main_ctx_fat_ptr, &main_ctx_fat_ptr_data)?;

        let main_ctx_gv = ctx
            .module_mut()
            .declare_data_in_func(main_ctx_id, &mut builder.func);
        let main_ctx_ptr = builder.ins().global_value(pointer_type, main_ctx_gv);
        let init_block_id = builder.ins().iconst(pointer_type, 0);
        builder.ins().store(
            MemFlags::new(),
            init_block_id,
            main_ctx_ptr,
            BLOCK_ID as i32,
        );
        builder.ins().store(
            MemFlags::new(),
            branch_id_val,
            main_ctx_ptr,
            BRANCH_ID as i32,
        );
        builder.ins().store(
            MemFlags::new(),
            main_vars_fat_ptr,
            main_ctx_ptr,
            VARIABLES as i32,
        );
        builder.ins().store(
            MemFlags::new(),
            main_args_fat_ptr,
            main_ctx_ptr,
            JUMP_ARGS as i32,
        );

        let main_ctx_fat_ptr_gv = ctx
            .module_mut()
            .declare_data_in_func(main_ctx_fat_ptr, &mut builder.func);
        let main_ctx_fat_ptr = builder
            .ins()
            .global_value(pointer_type, main_ctx_fat_ptr_gv);

        let end_ptr = builder.ins().iadd_imm(main_ctx_ptr, CTX_SIZE);
        builder
            .ins()
            .store(MemFlags::new(), main_ctx_ptr, main_ctx_fat_ptr, 0);
        builder
            .ins()
            .store(MemFlags::new(), end_ptr, main_ctx_fat_ptr, 8);

        let current_ctx = builder.declare_var(pointer_type);
        let steps = builder.declare_var(pointer_type);
        let zero = builder.ins().iconst(pointer_type, 0);
        builder.def_var(current_ctx, main_ctx_fat_ptr);
        builder.def_var(steps, zero);

        let action_block = builder.create_block();
        let after_action_block = builder.create_block();
        builder.append_block_param(after_action_block, pointer_type);
        let else_block = builder.create_block();
        let end_block = builder.create_block();
        let cond_block = builder.create_block();
        builder.ins().jump(cond_block, &[]);
        builder.switch_to_block(cond_block);

        let curr_steps_count = builder.use_var(steps);
        let cond = builder
            .ins()
            .icmp_imm(IntCC::UnsignedLessThan, curr_steps_count, 256);
        builder.ins().brif(cond, action_block, &[], else_block, &[]);

        builder.switch_to_block(action_block);
        let ctx_fat_ptr = builder.use_var(current_ctx);
        let res = builder.ins().call(main_ref, &[ctx_fat_ptr]);
        let next_block = builder.inst_results(res)[0];
        let cond = builder.ins().icmp_imm(IntCC::NotEqual, next_block, -1);
        builder.ins().brif(
            cond,
            after_action_block,
            &[BlockArg::Value(next_block)],
            end_block,
            &[],
        );

        builder.switch_to_block(after_action_block);
        let next_block_id = builder.block_params(after_action_block)[0];
        let ctx_fat_ptr = builder.use_var(current_ctx);
        let store_ref = ctx.get_func(&mut builder, "rt_store")?;
        let block_offset = builder.ins().iconst(pointer_type, BLOCK_ID);
        let size = builder.ins().iconst(pointer_type, 8);
        builder
            .ins()
            .call(store_ref, &[ctx_fat_ptr, next_block_id, size, block_offset]);
        let curr_steps_count = builder.use_var(steps);
        let inc_steps_count = builder.ins().iadd_imm(curr_steps_count, 1);
        builder.def_var(steps, inc_steps_count);
        builder.ins().jump(cond_block, &[]);

        builder.switch_to_block(else_block);
        let zero = builder.ins().iconst(pointer_type, 0);
        builder.def_var(steps, zero);
        builder.ins().jump(cond_block, &[]);

        builder.switch_to_block(end_block);
        let exit_ref = ctx.get_func(&mut builder, "rt_exit")?;

        let zero = builder.ins().iconst(pointer_type, 0);
        builder.ins().call(exit_ref, &[zero]);
        builder.ins().trap(TrapCode::user(0xDE).unwrap());

        builder.seal_all_blocks();

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
