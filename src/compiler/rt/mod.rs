use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, Linkage, Module},
    prelude::{
        FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
    },
};

use crate::compiler::{
    ctx::{CompilerCtx, rt_funcs::RtFuncs},
    rt::{
        funcs::{
            alloc::{define_allocate, define_loads, define_store},
            exit::define_exit,
            mmap::define_mmap,
            syscall::declare_syscall_wrapper,
            write::{define_write, define_write_static},
        },
        layout::{ExecCtxLayout, FatPtrLayout},
    },
};

pub mod funcs;
pub mod layout;
pub mod scheduler;

/// Builds the runtime layer of the compiled program.
///
/// Call `init()` before compiling any machines, then `build()` after all
/// machines have been compiled. `build()` emits the `_start` entry point that
/// contains the scheduler loop.
pub struct RuntimeBuilder;

impl RuntimeBuilder {
    /// Declare and define all runtime functions.  Must be called first.
    pub fn init(ctx: CompilerCtx) -> Result<CompilerCtx> {
        let ctx = declare_syscall_wrapper(ctx)?;
        let ctx = define_exit(ctx)?;
        let ctx = define_mmap(ctx)?;
        let ctx = define_allocate(ctx)?;
        let ctx = define_store(ctx)?;
        let ctx = define_loads(ctx)?;
        let ctx = define_write(ctx)?;
        let ctx = define_write_static(ctx)?;

        // Resolve and cache all FuncIds in the context.
        let mut ctx = ctx;
        let rt = RtFuncs::resolve(ctx.module())?;
        ctx.set_rt_funcs(rt);
        Ok(ctx)
    }

    /// Emit `_start` — the scheduler loop that drives the main machine.
    pub fn build(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let mut builder_ctx = FunctionBuilderContext::default();
        let mut module_ctx = ctx.module().make_context();
        let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);
        ctx.begin_function();

        let entry_block = builder.create_block();
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        // ── Resolve main branch metadata ──────────────────────────────────────
        let branch_id = ctx
            .lookup_param_count("main", 0)
            .ok_or_else(|| anyhow!("No zero-parameter branch in 'main'"))?;
        let var_count = ctx
            .lookup_vars_count(branch_id)
            .ok_or_else(|| anyhow!("Variable count for branch {branch_id} not found"))?;

        // ── Allocate the variables buffer ─────────────────────────────────────
        let (_main_vars_ptr, main_vars_fat_ptr) =
            alloc_static_buffer(&mut ctx, &mut builder, ptr_ty, "main_vars", var_count * 8)?;

        // ── Allocate the jump-arguments buffer (256 slots) ────────────────────
        let (_main_args_ptr, main_args_fat_ptr) =
            alloc_static_buffer(&mut ctx, &mut builder, ptr_ty, "main_args", 256 * 8)?;

        // ── Allocate the execution context ────────────────────────────────────
        let main_ctx_id =
            ctx.module_mut()
                .declare_data("main_ctx", Linkage::Export, true, false)?;
        let mut main_ctx_data = DataDescription::new();
        main_ctx_data.define_zeroinit(ExecCtxLayout::SIZE as usize);
        ctx.module_mut().define_data(main_ctx_id, &main_ctx_data)?;

        let main_ctx_fat_ptr_id =
            ctx.module_mut()
                .declare_data("main_ctx_fat_ptr", Linkage::Export, true, false)?;
        let mut fat_ptr_data = DataDescription::new();
        fat_ptr_data.define_zeroinit(FatPtrLayout::SIZE);
        ctx.module_mut()
            .define_data(main_ctx_fat_ptr_id, &fat_ptr_data)?;

        let main_ctx_gv = ctx
            .module_mut()
            .declare_data_in_func(main_ctx_id, &mut builder.func);
        let main_ctx_fat_ptr_gv = ctx
            .module_mut()
            .declare_data_in_func(main_ctx_fat_ptr_id, &mut builder.func);

        let main_ctx_ptr = builder.ins().global_value(ptr_ty, main_ctx_gv);
        let main_ctx_fat_ptr = builder.ins().global_value(ptr_ty, main_ctx_fat_ptr_gv);

        // Initialise ExecCtx fields.
        let branch_id_val = builder.ins().iconst(ptr_ty, branch_id as i64);
        let zero = builder.ins().iconst(ptr_ty, 0);
        ExecCtxLayout::store(&mut builder, branch_id_val, main_ctx_ptr, ExecCtxLayout::BRANCH_ID);
        ExecCtxLayout::store(&mut builder, zero, main_ctx_ptr, ExecCtxLayout::BLOCK_ID);
        ExecCtxLayout::store(&mut builder, main_vars_fat_ptr, main_ctx_ptr, ExecCtxLayout::VARIABLES);
        ExecCtxLayout::store(&mut builder, main_args_fat_ptr, main_ctx_ptr, ExecCtxLayout::JUMP_ARGS);

        // Initialise ctx fat pointer.
        let ctx_end = builder.ins().iadd_imm(main_ctx_ptr, ExecCtxLayout::SIZE as i64);
        FatPtrLayout::store_start(&mut builder, main_ctx_fat_ptr, main_ctx_ptr);
        FatPtrLayout::store_end(&mut builder, main_ctx_fat_ptr, ctx_end);

        // ── Resolve main function ref ─────────────────────────────────────────
        let main_ref = ctx.get_func(&mut builder, "main")?;

        // ── Scheduler loop ────────────────────────────────────────────────────
        // Structure:
        //   loop_block:
        //     next_block_id = main(ctx_fat_ptr)
        //     if next_block_id == -1: goto exit_block
        //     store next_block_id into ctx.BLOCK_ID
        //     goto loop_block
        //   exit_block:
        //     rt_exit(0)

        let loop_block = builder.create_block();
        let exit_block = builder.create_block();
        let continue_block = builder.create_block();
        builder.append_block_param(continue_block, ptr_ty); // next_block_id

        builder.ins().jump(loop_block, &[]);

        builder.switch_to_block(loop_block);
        let call = builder.ins().call(main_ref, &[main_ctx_fat_ptr]);
        let next_block_id = builder.inst_results(call)[0];
        let is_done = builder
            .ins()
            .icmp_imm(IntCC::Equal, next_block_id, -1);
        builder.ins().brif(
            is_done,
            exit_block,
            &[],
            continue_block,
            &[BlockArg::Value(next_block_id)],
        );

        builder.switch_to_block(continue_block);
        let next_id = builder.block_params(continue_block)[0];
        let store_ref = ctx.get_func(&mut builder, "rt_store")?;
        let block_id_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::BLOCK_ID as i64);
        let size = builder.ins().iconst(ptr_ty, 8);
        builder
            .ins()
            .call(store_ref, &[main_ctx_fat_ptr, next_id, size, block_id_offset]);
        builder.ins().jump(loop_block, &[]);

        builder.switch_to_block(exit_block);
        let exit_ref = ctx.get_func(&mut builder, "rt_exit")?;
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder.ins().call(exit_ref, &[zero]);
        builder.ins().trap(TrapCode::user(0xDE).unwrap());

        builder.seal_all_blocks();

        let sig = builder.func.signature.clone();
        let id = ctx
            .module_mut()
            .declare_function("_start", Linkage::Export, &sig)?;
        ctx.module_mut().define_function(id, &mut module_ctx)?;
        ctx.module_mut().clear_context(&mut module_ctx);

        Ok(ctx)
    }
}

/// Allocate a zero-initialised static buffer and return
/// `(data_ptr, fat_ptr_global_value)` where both are Cranelift `Value`s
/// ready for use in the current function.
fn alloc_static_buffer(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ptr_ty: cranelift::prelude::Type,
    name: &str,
    byte_size: usize,
) -> Result<(cranelift::prelude::Value, cranelift::prelude::Value)> {
    let data_id = ctx
        .module_mut()
        .declare_data(name, Linkage::Export, true, false)?;
    let mut data_desc = DataDescription::new();
    data_desc.define_zeroinit(byte_size);
    ctx.module_mut().define_data(data_id, &data_desc)?;

    let fat_ptr_name = format!("{name}_fat_ptr");
    let fat_ptr_id = ctx
        .module_mut()
        .declare_data(&fat_ptr_name, Linkage::Export, true, false)?;
    let mut fat_desc = DataDescription::new();
    fat_desc.define_zeroinit(FatPtrLayout::SIZE);
    ctx.module_mut().define_data(fat_ptr_id, &fat_desc)?;

    let data_gv = ctx
        .module_mut()
        .declare_data_in_func(data_id, &mut builder.func);
    let fat_ptr_gv = ctx
        .module_mut()
        .declare_data_in_func(fat_ptr_id, &mut builder.func);

    let data_ptr = builder.ins().global_value(ptr_ty, data_gv);
    let fat_ptr = builder.ins().global_value(ptr_ty, fat_ptr_gv);

    let end = builder.ins().iadd_imm(data_ptr, byte_size as i64);
    builder.ins().store(MemFlags::new(), data_ptr, fat_ptr, 0);
    builder.ins().store(MemFlags::new(), end, fat_ptr, 8);

    Ok((data_ptr, fat_ptr))
}
