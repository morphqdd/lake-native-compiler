use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, Linkage, Module},
    prelude::{Block, FunctionBuilder, InstBuilder, IntCC, Type, Value, Variable},
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::{
        alloc_static_buffer, get_static_buffer,
        layout::{ExecCtxLayout, FatPtrLayout, process_ctx::ProcessCtxLayout},
    },
};

pub struct ShedulerCtxLayout;

impl ShedulerCtxLayout {
    /// Declare and zero-initialise the scheduler and process-array data sections
    /// in the module **before** any machines are compiled, so that `spawn_expr`
    /// can reference them as global symbols during machine compilation.
    pub fn declare_globals(ctx: &mut CompilerCtx) -> Result<()> {
        let module = ctx.module_mut();

        for (name, size) in [
            ("sheduler_ctx", Self::SIZE as usize),
            ("sheduler_ctx_fat_ptr", FatPtrLayout::SIZE),
            ("process_arr", 256 * 8),
            ("process_arr_fat_ptr", FatPtrLayout::SIZE),
        ] {
            let id = module.declare_data(name, Linkage::Export, true, false)?;
            let mut desc = DataDescription::new();
            desc.define_zeroinit(size);
            module.define_data(id, &desc)?;
        }
        Ok(())
    }

    pub const SIZE: i32 = 48;
    pub const PROCESS_ARR_FAT: i32 = 0;
    pub const CURRENT_PROCESS: i32 = 8;
    pub const LAST_PROCESS_INDEX: i32 = 16;
    pub const REDUCTION_LIMIT: i32 = 24;
    pub const REAL_COUNT_OF_PROCESSES: i32 = 32;
    pub const REDUCTION_COUNTER: i32 = 40;

    pub const REDUCTION_LIMIT_VALUE: i64 = 1000;

    pub fn init(
        ctx: &mut crate::compiler::ctx::CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Variable> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_funcs = ctx.rt_funcs().clone();
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);

        let (_, sh_ctx_ptr) = get_static_buffer(
            ctx,
            builder,
            ptr_ty,
            "sheduler_ctx",
            ShedulerCtxLayout::SIZE as usize,
        )?;

        let (_, process_arr_ptr) = get_static_buffer(ctx, builder, ptr_ty, "process_arr", 256 * 8)?;

        let process_arr_offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_FAT as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, process_arr_ptr, ptr_size, process_arr_offset],
        );

        let reduction_limit = builder.ins().iconst(ptr_ty, Self::REDUCTION_LIMIT_VALUE);
        let reduction_limit_offset = builder.ins().iconst(ptr_ty, Self::REDUCTION_LIMIT as i64);
        builder.ins().call(
            store_ref,
            &[
                sh_ctx_ptr,
                reduction_limit,
                ptr_size,
                reduction_limit_offset,
            ],
        );

        let var = builder.declare_var(ptr_ty);
        builder.def_var(var, sh_ctx_ptr);
        Ok(var)
    }

    pub fn init_main_process(
        sh_ptr_var: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();

        let branch_id = ctx
            .lookup_param_count("main", 0)
            .ok_or_else(|| anyhow!("No zero-parameter branch in 'main'"))?;

        // Size the buffer by the maximum var_count across all branches of main
        // so that state transitions never overflow the variables array.
        let max_vars = ctx
            .max_branch_var_count("main")
            .ok_or_else(|| anyhow!("No branches found in 'main'"))?
            .max(1);

        // ── Allocate the variables buffer ─────────────────────────────────────
        let (_main_vars_ptr, main_vars_fat_ptr) =
            alloc_static_buffer(ctx, builder, ptr_ty, "main_vars", max_vars * 8)?;

        // ── Allocate the jump-arguments buffer (256 slots) ────────────────────
        let (_main_args_ptr, main_args_fat_ptr) =
            alloc_static_buffer(ctx, builder, ptr_ty, "main_args", 256 * 8)?;

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

        let branch_id_val = builder.ins().iconst(ptr_ty, branch_id as i64);
        let zero = builder.ins().iconst(ptr_ty, 0);
        ExecCtxLayout::store(
            builder,
            branch_id_val,
            main_ctx_ptr,
            ExecCtxLayout::BRANCH_ID,
        );
        ExecCtxLayout::store(builder, zero, main_ctx_ptr, ExecCtxLayout::BLOCK_ID);
        ExecCtxLayout::store(
            builder,
            main_vars_fat_ptr,
            main_ctx_ptr,
            ExecCtxLayout::VARIABLES,
        );
        ExecCtxLayout::store(
            builder,
            main_args_fat_ptr,
            main_ctx_ptr,
            ExecCtxLayout::JUMP_ARGS,
        );

        let ctx_end = builder
            .ins()
            .iadd_imm(main_ctx_ptr, ExecCtxLayout::SIZE as i64);
        FatPtrLayout::store_start(builder, main_ctx_fat_ptr, main_ctx_ptr);
        FatPtrLayout::store_end(builder, main_ctx_fat_ptr, ctx_end);

        let process_ctx = ProcessCtxLayout::init_ctx(ctx, builder, "main", main_ctx_fat_ptr)?;

        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let process_arr_offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_FAT as i64);
        let first_index = builder.ins().iconst(ptr_ty, 0);

        let call_load_process_arr = builder
            .ins()
            .call(load_ref, &[sh_ctx_ptr, process_arr_offset]);
        let process_arr = builder.inst_results(call_load_process_arr)[0];

        builder.ins().call(
            store_ref,
            &[process_arr, process_ctx, ptr_size, first_index],
        );

        // Mark one active process so the scheduler loop doesn't exit immediately.
        let real_count_offset = builder
            .ins()
            .iconst(ptr_ty, Self::REAL_COUNT_OF_PROCESSES as i64);
        let one = builder.ins().iconst(ptr_ty, 1);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, one, ptr_size, real_count_offset]);

        Ok(())
    }

    pub fn increment_reduction_counter(
        sh_ptr_var: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_func = ctx.rt_funcs().clone();
        let load_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_func.store_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        let offset = builder.ins().iconst(ptr_ty, Self::REDUCTION_COUNTER as i64);
        let call = builder.ins().call(load_ref, &[sh_ctx_ptr, offset]);
        let counter = builder.inst_results(call)[0];
        let new_counter = builder.ins().iadd_imm(counter, 1);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_counter, ptr_size, offset]);
    }

    pub fn get_real_count_of_processes(
        sh_ctx_ptr: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES as i64);
        let call_load_real_count_of_processes =
            builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let real_count_of_processes = builder.inst_results(call_load_real_count_of_processes)[0];
        Ok(real_count_of_processes)
    }

    pub fn get_reduction_counter(
        sh_ctx_ptr: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::REDUCTION_COUNTER as i64);
        let call_load_reduction_counter = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let reduction_counter = builder.inst_results(call_load_reduction_counter)[0];
        Ok(reduction_counter)
    }

    pub fn get_reduction_limit(
        sh_ctx_ptr: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::REDUCTION_LIMIT as i64);
        let call_load_reduction_limit = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let reduction_limit = builder.inst_results(call_load_reduction_limit)[0];
        Ok(reduction_limit)
    }

    pub fn get_current_process(
        sh_ctx_ptr: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Value> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::CURRENT_PROCESS as i64);
        let current_process_index_call = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let current_process_index = builder.inst_results(current_process_index_call)[0];
        let aligned_index = builder.ins().imul_imm(current_process_index, 8);

        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::PROCESS_ARR_FAT as i64);
        let call_process_arr_ptr = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let process_arr_ptr = builder.inst_results(call_process_arr_ptr)[0];

        let call_load_process = builder
            .ins()
            .call(load_func_ref, &[process_arr_ptr, aligned_index]);
        let current_process = builder.inst_results(call_load_process)[0];

        Ok(current_process)
    }
    /// Remove the current process using swap-and-pop so the array stays dense.
    ///
    /// The last element is copied into the vacated slot; the now-empty tail is
    /// zeroed and `LAST_PROCESS_INDEX` is decremented.  If the removed process
    /// happened to be the last slot (`current == last`), `CURRENT_PROCESS` is
    /// reset to 0 so the next iteration doesn't chase a stale pointer.
    ///
    /// Emits its own terminating jump to `loop_block` (both branches of the
    /// conditional converge there), so the caller must NOT emit a jump after
    /// this call.
    pub fn remove_current_process(
        sh_ptr_var: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
        loop_block: Block,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_func = ctx.rt_funcs().clone();
        let load_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_func.store_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        // ── Load indices ─────────────────────────────────────────────────────
        let current_offset = builder.ins().iconst(ptr_ty, Self::CURRENT_PROCESS as i64);
        let call_current = builder.ins().call(load_ref, &[sh_ctx_ptr, current_offset]);
        let current_idx = builder.inst_results(call_current)[0];
        let current_aligned = builder.ins().imul_imm(current_idx, 8);

        let last_offset = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_PROCESS_INDEX as i64);
        let call_last = builder.ins().call(load_ref, &[sh_ctx_ptr, last_offset]);
        let last_idx = builder.inst_results(call_last)[0];
        let last_aligned = builder.ins().imul_imm(last_idx, 8);

        // ── Load process array ────────────────────────────────────────────────
        let arr_offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_FAT as i64);
        let call_arr = builder.ins().call(load_ref, &[sh_ctx_ptr, arr_offset]);
        let process_arr = builder.inst_results(call_arr)[0];

        // ── Swap-and-pop: copy last → current, zero last ──────────────────────
        let call_last_proc = builder.ins().call(load_ref, &[process_arr, last_aligned]);
        let last_proc = builder.inst_results(call_last_proc)[0];
        builder.ins().call(
            store_ref,
            &[process_arr, last_proc, ptr_size, current_aligned],
        );
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder
            .ins()
            .call(store_ref, &[process_arr, zero, ptr_size, last_aligned]);

        // ── Shrink the array ──────────────────────────────────────────────────
        let new_last = builder.ins().iadd_imm(last_idx, -1);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_last, ptr_size, last_offset]);

        // ── Decrement active count ────────────────────────────────────────────
        let real_count_offset = builder
            .ins()
            .iconst(ptr_ty, Self::REAL_COUNT_OF_PROCESSES as i64);
        let call_count = builder
            .ins()
            .call(load_ref, &[sh_ctx_ptr, real_count_offset]);
        let real_count = builder.inst_results(call_count)[0];
        let new_count = builder.ins().iadd_imm(real_count, -1);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_count, ptr_size, real_count_offset],
        );

        // ── Fix CURRENT_PROCESS if we just removed the last slot ──────────────
        // After swap-and-pop, if current == last, the slot is now zeroed and
        // CURRENT_PROCESS would point past the valid range. Reset it to 0.
        let reset_block = builder.create_block();
        let done_block = builder.create_block();

        let was_last = builder.ins().icmp(IntCC::Equal, current_idx, last_idx);
        builder
            .ins()
            .brif(was_last, reset_block, &[], done_block, &[]);

        builder.switch_to_block(reset_block);
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);
        let zero = builder.ins().iconst(ptr_ty, 0);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let current_offset = builder.ins().iconst(ptr_ty, Self::CURRENT_PROCESS as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, zero, ptr_size, current_offset]);
        builder.ins().jump(loop_block, &[]);

        builder.switch_to_block(done_block);
        builder.ins().jump(loop_block, &[]);

        Ok(())
    }

    pub fn new_process(
        sh_ctx_ptr: Value,
        process_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_FAT as i64);
        let call_process_arr = builder.ins().call(load_ref, &[sh_ctx_ptr, offset]);
        let process_arr = builder.inst_results(call_process_arr)[0];

        let offset_last_i = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_PROCESS_INDEX as i64);
        let call_last_index = builder.ins().call(load_ref, &[sh_ctx_ptr, offset_last_i]);
        let last_process_index = builder.inst_results(call_last_index)[0];
        let next_process_index = builder.ins().iadd_imm(last_process_index, 1);
        let aligned_index = builder.ins().imul_imm(next_process_index, 8);

        builder.ins().call(
            store_ref,
            &[process_arr, process_ctx_ptr, ptr_size, aligned_index],
        );

        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, next_process_index, ptr_size, offset_last_i],
        );

        // Increment REAL_COUNT_OF_PROCESSES so the scheduler doesn't exit early.
        let real_count_offset = builder
            .ins()
            .iconst(ptr_ty, Self::REAL_COUNT_OF_PROCESSES as i64);
        let call_count = builder
            .ins()
            .call(load_ref, &[sh_ctx_ptr, real_count_offset]);
        let real_count = builder.inst_results(call_count)[0];
        let new_count = builder.ins().iadd_imm(real_count, 1);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_count, ptr_size, real_count_offset],
        );

        Ok(())
    }

    pub fn next_process(
        sh_ctx_var: Variable,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
        after_block: Block,
    ) {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_func = ctx.rt_funcs().clone();
        let load_func_ref = rt_func.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_func.store_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_var);

        // Reset reduction counter when switching processes.
        let zero = builder.ins().iconst(ptr_ty, 0);
        let counter_offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::REDUCTION_COUNTER as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, zero, ptr_size, counter_offset]);

        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::CURRENT_PROCESS as i64);
        let call_load_current_process_index =
            builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let current_process_index = builder.inst_results(call_load_current_process_index)[0];

        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::LAST_PROCESS_INDEX as i64);
        let call_load_last_process_index = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let last_process_index = builder.inst_results(call_load_last_process_index)[0];

        let next_process_index = builder.ins().iadd_imm(current_process_index, 1);
        let is_eq = builder.ins().icmp(
            IntCC::UnsignedLessThan,
            last_process_index,
            next_process_index,
        );

        let reset_block = builder.create_block();
        let inc_block = builder.create_block();
        builder.append_block_param(inc_block, ptr_ty);

        builder.ins().brif(
            is_eq,
            reset_block,
            &[],
            inc_block,
            &[BlockArg::Value(next_process_index)],
        );

        builder.switch_to_block(reset_block);
        let sh_ctx_ptr = builder.use_var(sh_ctx_var);
        let zero = builder.ins().iconst(ptr_ty, 0);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::CURRENT_PROCESS as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, zero, ptr_size, offset]);
        builder.ins().jump(after_block, &[]);

        builder.switch_to_block(inc_block);
        let next_proccess = builder.block_params(inc_block)[0];
        let sh_ctx_ptr = builder.use_var(sh_ctx_var);
        let offset = builder
            .ins()
            .iconst(ptr_ty, ShedulerCtxLayout::CURRENT_PROCESS as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, next_proccess, ptr_size, offset]);
        builder.ins().jump(after_block, &[]);
    }
}
