use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{DataDescription, Linkage, Module},
    prelude::{Block, FunctionBuilder, InstBuilder, IntCC, MemFlags, Type, Value, Variable},
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

        // process_arr / wait_arr now live on the heap (allocated via
        // rt_allocate in `init`) so they can grow on demand — no static
        // backing buffers here.  The scheduler context itself remains a
        // single static struct to avoid a chicken-and-egg with the heap.
        for (name, size) in [
            ("sheduler_ctx", Self::SIZE as usize),
            ("sheduler_ctx_fat_ptr", FatPtrLayout::SIZE),
        ] {
            let id = module.declare_data(name, Linkage::Export, true, false)?;
            let mut desc = DataDescription::new();
            desc.define_zeroinit(size);
            module.define_data(id, &desc)?;
        }
        Ok(())
    }

    pub const SIZE: i32 = 176;
    pub const PROCESS_ARR_FAT: i32 = 0;
    pub const CURRENT_PROCESS: i32 = 8;
    pub const LAST_PROCESS_INDEX: i32 = 16;
    pub const REAL_COUNT_OF_PROCESSES: i32 = 24;
    pub const WAIT_ARR_FAT: i32 = 32;
    pub const LAST_WAITED_PROCESS_INDEX: i32 = 40;
    pub const WAITED_PROCESS_COUNT: i32 = 48;
    /// Current capacity (in slots) of `process_arr`.  Doubles when the queue
    /// reaches the cap; the old buffer is freed back to the allocator.
    pub const PROCESS_ARR_CAP: i32 = 56;
    /// Current capacity (in slots) of `wait_arr`.  Same growth strategy.
    pub const WAIT_ARR_CAP: i32 = 64;
    // ── io_uring (added in stage 2) ─────────────────────────────────────────
    /// `io_uring_setup` returns a kernel fd for the ring.
    pub const IO_URING_FD: i32 = 72;
    /// Resolved address of the SQ tail counter (u32) — `mmap_sq + sq_off.tail`.
    pub const SQ_TAIL_PTR: i32 = 80;
    /// Resolved address of the SQ ring mask (u32) — `mmap_sq + sq_off.ring_mask`.
    pub const SQ_MASK_PTR: i32 = 88;
    /// Resolved address of the SQ array (u32 entries) — `mmap_sq + sq_off.array`.
    pub const SQ_ARRAY_PTR: i32 = 96;
    /// mmap base of the SQE array (256 × 64 B entries).
    pub const SQE_ARRAY_PTR: i32 = 104;
    /// Resolved address of the CQ head counter (u32).
    pub const CQ_HEAD_PTR: i32 = 112;
    /// Resolved address of the CQ tail counter (u32).
    pub const CQ_TAIL_PTR: i32 = 120;
    /// Resolved address of the CQ ring mask (u32).
    pub const CQ_MASK_PTR: i32 = 128;
    /// Resolved address of the CQE array (16 B entries).
    pub const CQ_CQES_PTR: i32 = 136;
    /// Heap fat-ptr to the parked-actor list.  Stride = 16 B = (proc_ctx, _).
    pub const IO_PARKED_FAT: i32 = 144;
    pub const IO_PARKED_COUNT: i32 = 152;
    pub const IO_PARKED_CAP: i32 = 160;
    /// Pending SQEs not yet submitted via `io_uring_enter`.  rt_write_async
    /// increments this on every submit; when it reaches `SQE_BATCH_SIZE`,
    /// the helper calls `io_uring_enter(fd, count, …)` and resets to 0.
    /// A scheduler-level flush will drain the residual on quantum boundaries
    /// (stage 5).
    pub const SQE_PENDING: i32 = 168;

    pub const INITIAL_QUEUE_CAP: i64 = 256;
    /// Initial parked-actor list capacity (pairs).  Grows with the same
    /// doubling helper as `process_arr` / `wait_arr` (stride parameterised).
    /// Initial capacity 4096 to absorb up to 4k concurrently-parked actors
    /// without grow.  TODO: add grow path same as process_arr/wait_arr; for
    /// now this caps practical concurrency at 4096.
    pub const INITIAL_IO_PARKED_CAP: i64 = 4096;

    pub fn init(
        ctx: &mut crate::compiler::ctx::CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<Variable> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let rt_funcs = ctx.rt_funcs().clone();
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let allocate_ref = rt_funcs.allocate_ref(ctx.module_mut(), builder);

        let (_, sh_ctx_ptr) = get_static_buffer(
            ctx,
            builder,
            ptr_ty,
            "sheduler_ctx",
            ShedulerCtxLayout::SIZE as usize,
        )?;

        // process_arr — heap-allocated fat-ptr that grows on demand.
        let init_bytes = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP * 8);
        let call_pa = builder.ins().call(allocate_ref, &[init_bytes]);
        let process_arr_ptr = builder.inst_results(call_pa)[0];
        let process_arr_offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_FAT as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, process_arr_ptr, ptr_size, process_arr_offset],
        );

        let init_cap = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP);
        let process_cap_offset = builder.ins().iconst(ptr_ty, Self::PROCESS_ARR_CAP as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, init_cap, ptr_size, process_cap_offset],
        );

        // wait_arr — same growth strategy as process_arr.
        let init_bytes = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP * 8);
        let call_wa = builder.ins().call(allocate_ref, &[init_bytes]);
        let wait_arr_ptr = builder.inst_results(call_wa)[0];
        let wait_arr_offset = builder.ins().iconst(ptr_ty, Self::WAIT_ARR_FAT as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, wait_arr_ptr, ptr_size, wait_arr_offset],
        );

        let init_cap = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP);
        let wait_cap_offset = builder.ins().iconst(ptr_ty, Self::WAIT_ARR_CAP as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, init_cap, ptr_size, wait_cap_offset],
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

        // All main-process resources are heap-allocated (rt_allocate) — uniform
        // with spawned processes.  This makes process-death cleanup trivial:
        // call rt_free on every fat-ptr regardless of who owns the process.
        // Future opt-in `@static main` will trade reclamation for zero alloc.
        let rt_funcs = ctx.rt_funcs().clone();
        let allocate_ref = rt_funcs.allocate_ref(ctx.module_mut(), builder);

        let vars_size = builder.ins().iconst(ptr_ty, (max_vars * 8) as i64);
        let call_vars = builder.ins().call(allocate_ref, &[vars_size]);
        let main_vars_fat_ptr = builder.inst_results(call_vars)[0];

        let args_size = builder.ins().iconst(ptr_ty, 256 * 8);
        let call_args = builder.ins().call(allocate_ref, &[args_size]);
        let main_args_fat_ptr = builder.inst_results(call_args)[0];

        let mb_size = builder.ins().iconst(ptr_ty, 256 * 8);
        let call_mb = builder.ins().call(allocate_ref, &[mb_size]);
        let main_mb_fat_ptr = builder.inst_results(call_mb)[0];

        let exec_ctx_size = builder.ins().iconst(ptr_ty, ExecCtxLayout::SIZE as i64);
        let call_ctx = builder.ins().call(allocate_ref, &[exec_ctx_size]);
        let main_ctx_fat_ptr = builder.inst_results(call_ctx)[0];

        // Dereference the fat-ptr once to get the raw ExecCtx address; init fields.
        let main_ctx_ptr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), main_ctx_fat_ptr, 0);

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
        ExecCtxLayout::store(
            builder,
            main_mb_fat_ptr,
            main_ctx_ptr,
            ExecCtxLayout::MAILBOX_FAT,
        );
        ExecCtxLayout::store(builder, zero, main_ctx_ptr, ExecCtxLayout::MAILBOX_HEAD);
        ExecCtxLayout::store(builder, zero, main_ctx_ptr, ExecCtxLayout::MAILBOX_TAIL);

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

    /// Return all heap allocations associated with a dead process to the
    /// allocator's free list.
    ///
    /// Layout assumed:
    ///   ProcessCtx fat-ptr → {FUNC_PTR @0, EXEC_CTX @8}
    ///   ExecCtx fat-ptr    → {…, VARIABLES @24, JUMP_ARGS @32, MAILBOX @40, …}
    ///
    /// Reads every nested fat-ptr **before** freeing the parent — once a
    /// fat-ptr is on the free list its payload's first 8 bytes are clobbered
    /// with the chain pointer, so any fields we needed must already be in
    /// registers.
    pub fn free_process_resources(
        process_ctx_fat_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let free_ref = rt_funcs.free_ref(ctx.module_mut(), builder);

        // Read EXEC_CTX (offset 8) from the ProcessCtx payload.
        let exec_ctx_offset = builder.ins().iconst(ptr_ty, ProcessCtxLayout::EXEC_CTX as i64);
        let call_exec = builder
            .ins()
            .call(load_ref, &[process_ctx_fat_ptr, exec_ctx_offset]);
        let exec_ctx_fat_ptr = builder.inst_results(call_exec)[0];

        // Read the three nested fat-ptrs from the ExecCtx payload before any
        // free, since freeing them clobbers their start fields.
        let vars_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
        let call_vars = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, vars_offset]);
        let vars_fat_ptr = builder.inst_results(call_vars)[0];

        let args_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let call_args = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, args_offset]);
        let args_fat_ptr = builder.inst_results(call_args)[0];

        let mb_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::MAILBOX_FAT as i64);
        let call_mb = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, mb_offset]);
        let mailbox_fat_ptr = builder.inst_results(call_mb)[0];

        // Free leaf allocations first, then the containers.
        builder.ins().call(free_ref, &[vars_fat_ptr]);
        builder.ins().call(free_ref, &[args_fat_ptr]);
        builder.ins().call(free_ref, &[mailbox_fat_ptr]);
        builder.ins().call(free_ref, &[exec_ctx_fat_ptr]);
        builder.ins().call(free_ref, &[process_ctx_fat_ptr]);
    }

    pub fn get_waited_processes(
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
            .iconst(ptr_ty, ShedulerCtxLayout::WAITED_PROCESS_COUNT as i64);
        let call = builder.ins().call(load_func_ref, &[sh_ctx_ptr, offset]);
        let count = builder.inst_results(call)[0];
        Ok(count)
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

    pub fn wait_current_process(
        sh_ptr_var: Variable,
        process_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        let offset_last_i = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_WAITED_PROCESS_INDEX as i64);
        let call_last_index = builder.ins().call(load_ref, &[sh_ctx_ptr, offset_last_i]);
        let last_process_index = builder.inst_results(call_last_index)[0];
        let next_process_index = builder.ins().iadd_imm(last_process_index, 1);
        let aligned_index = builder.ins().imul_imm(next_process_index, 8);

        // Grow wait_arr if next_index would exceed cap; use the (possibly
        // new) fat-ptr returned for the store below.
        let wait_arr = Self::emit_grow_array_if_full(
            sh_ctx_ptr,
            Self::WAIT_ARR_FAT,
            Self::WAIT_ARR_CAP,
            next_process_index,
            ctx,
            builder,
        );

        builder.ins().call(
            store_ref,
            &[wait_arr, process_ctx_ptr, ptr_size, aligned_index],
        );

        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, next_process_index, ptr_size, offset_last_i],
        );

        // Increment WAITED_PROCESS_COUNT
        let count_offset = builder
            .ins()
            .iconst(ptr_ty, Self::WAITED_PROCESS_COUNT as i64);
        let call_count = builder
            .ins()
            .call(load_ref, &[sh_ctx_ptr, count_offset]);
        let waited_count = builder.inst_results(call_count)[0];
        let new_count = builder.ins().iadd_imm(waited_count, 1);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_count, ptr_size, count_offset],
        );

        Ok(())
    }
    /// If the array referenced by `(fat_offset, cap_offset)` in the scheduler
    /// context cannot fit a slot at `next_index`, grow it to 2× capacity:
    /// allocate a new buffer via `rt_allocate`, copy the first `next_index`
    /// slots (8 bytes each) from old to new, free the old fat-ptr, and update
    /// both the FAT and CAP fields in `sh_ctx`.
    ///
    /// Returns the array fat-ptr to use for the subsequent store — this is
    /// either the original (no grow) or the freshly-allocated one.  Callers
    /// **must** use the returned value; any previously-loaded fat-ptr may
    /// dangle once `rt_free(old)` runs.
    fn emit_grow_array_if_full(
        sh_ctx_ptr: Value,
        fat_offset: i32,
        cap_offset: i32,
        next_index: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Value {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let allocate_ref = rt_funcs.allocate_ref(ctx.module_mut(), builder);
        let free_ref = rt_funcs.free_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let cap_off_v = builder.ins().iconst(ptr_ty, cap_offset as i64);
        let call_cap = builder.ins().call(load_ref, &[sh_ctx_ptr, cap_off_v]);
        let cap = builder.inst_results(call_cap)[0];

        let fat_off_v = builder.ins().iconst(ptr_ty, fat_offset as i64);
        let call_fat = builder.ins().call(load_ref, &[sh_ctx_ptr, fat_off_v]);
        let old_fat = builder.inst_results(call_fat)[0];

        let need_grow = builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, next_index, cap);

        let grow_block = builder.create_block();
        let no_grow_block = builder.create_block();
        let merge_block = builder.create_block();
        builder.append_block_param(merge_block, ptr_ty);

        builder
            .ins()
            .brif(need_grow, grow_block, &[], no_grow_block, &[]);

        // ── no_grow_block: pass old_fat through ─────────────────────────────
        builder.switch_to_block(no_grow_block);
        builder.seal_block(no_grow_block);
        builder
            .ins()
            .jump(merge_block, &[BlockArg::Value(old_fat)]);

        // ── grow_block: alloc 2× → copy → free → update fields ──────────────
        builder.switch_to_block(grow_block);
        builder.seal_block(grow_block);

        let new_cap = builder.ins().ishl_imm(cap, 1);
        let alloc_bytes = builder.ins().ishl_imm(new_cap, 3);
        let call_alloc = builder.ins().call(allocate_ref, &[alloc_bytes]);
        let new_fat = builder.inst_results(call_alloc)[0];

        // Copy first `next_index` slots (8 bytes each) old → new via inline loop.
        let old_start = builder.ins().load(ptr_ty, MemFlags::trusted(), old_fat, 0);
        let new_start = builder.ins().load(ptr_ty, MemFlags::trusted(), new_fat, 0);

        let copy_header = builder.create_block();
        let copy_body = builder.create_block();
        let copy_done = builder.create_block();
        builder.append_block_param(copy_header, ptr_ty);

        let zero = builder.ins().iconst(ptr_ty, 0);
        builder.ins().jump(copy_header, &[BlockArg::Value(zero)]);

        builder.switch_to_block(copy_header);
        let i = builder.block_params(copy_header)[0];
        let cmp = builder.ins().icmp(IntCC::UnsignedLessThan, i, next_index);
        builder.ins().brif(cmp, copy_body, &[], copy_done, &[]);

        builder.switch_to_block(copy_body);
        builder.seal_block(copy_body);
        let off = builder.ins().ishl_imm(i, 3);
        let src_addr = builder.ins().iadd(old_start, off);
        let dst_addr = builder.ins().iadd(new_start, off);
        let val = builder.ins().load(ptr_ty, MemFlags::trusted(), src_addr, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), val, dst_addr, 0);
        let i_next = builder.ins().iadd_imm(i, 1);
        builder.ins().jump(copy_header, &[BlockArg::Value(i_next)]);

        builder.switch_to_block(copy_done);
        builder.seal_block(copy_done);
        builder.seal_block(copy_header);

        builder.ins().call(free_ref, &[old_fat]);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_fat, ptr_size, fat_off_v]);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_cap, ptr_size, cap_off_v]);

        builder
            .ins()
            .jump(merge_block, &[BlockArg::Value(new_fat)]);

        builder.switch_to_block(merge_block);
        builder.seal_block(merge_block);
        builder.block_params(merge_block)[0]
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

        let offset_last_i = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_PROCESS_INDEX as i64);
        let call_last_index = builder.ins().call(load_ref, &[sh_ctx_ptr, offset_last_i]);
        let last_process_index = builder.inst_results(call_last_index)[0];
        let next_process_index = builder.ins().iadd_imm(last_process_index, 1);
        let aligned_index = builder.ins().imul_imm(next_process_index, 8);

        // Grow process_arr if next_index would exceed cap; use the (possibly
        // new) fat-ptr returned from the helper for the store below.
        let process_arr = Self::emit_grow_array_if_full(
            sh_ctx_ptr,
            Self::PROCESS_ARR_FAT,
            Self::PROCESS_ARR_CAP,
            next_process_index,
            ctx,
            builder,
        );

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

    /// Wake a process: scan `wait_arr` for `pid_val`, swap-remove it, and add
    /// it back to the active `process_arr` via `new_process`.
    ///
    /// If the PID is not found in `wait_arr` (process already active or not
    /// waiting), this is a no-op — the message is already in the mailbox and
    /// will be consumed when the receiver next checks.
    ///
    /// Emits a Cranelift loop that scans wait_arr[0..LAST_WAITED_PROCESS_INDEX].
    pub fn wake_process(
        sh_ptr_var: Variable,
        pid_val: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
        after_block: Block,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        // Load wait_arr pointer and LAST_WAITED_PROCESS_INDEX
        let wait_arr_offset = builder.ins().iconst(ptr_ty, Self::WAIT_ARR_FAT as i64);
        let call_wait_arr = builder.ins().call(load_ref, &[sh_ctx_ptr, wait_arr_offset]);
        let wait_arr = builder.inst_results(call_wait_arr)[0];

        let last_idx_offset = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_WAITED_PROCESS_INDEX as i64);
        let call_last_idx = builder.ins().call(load_ref, &[sh_ctx_ptr, last_idx_offset]);
        let last_waited_idx = builder.inst_results(call_last_idx)[0];

        // Check if wait_arr is empty (last_waited < 0 means no entries).
        // LAST_WAITED_PROCESS_INDEX starts at 0 and increments per entry,
        // but after all removed it wraps to -1 (unsigned large).
        // Use: if last_waited_idx + 1 == 0 → no waited processes → skip.
        let waited_count = builder.ins().iadd_imm(last_waited_idx, 1);
        let no_waited = builder.ins().icmp_imm(IntCC::Equal, waited_count, 0);

        let scan_block = builder.create_block();
        builder.append_block_param(scan_block, ptr_ty); // i (scan index)

        let found_block = builder.create_block();
        builder.append_block_param(found_block, ptr_ty); // found index

        let not_found_block = builder.create_block();

        // If no waited processes, skip directly
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder.ins().brif(
            no_waited,
            after_block,
            &[],
            scan_block,
            &[BlockArg::Value(zero)],
        );

        // ── scan_block(i): compare wait_arr[i] with pid ────────────────
        builder.switch_to_block(scan_block);
        let i = builder.block_params(scan_block)[0];
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        // Reload wait_arr (need fresh refs after block switch)
        let wait_arr_offset = builder.ins().iconst(ptr_ty, Self::WAIT_ARR_FAT as i64);
        let call_wait_arr = builder.ins().call(load_ref, &[sh_ctx_ptr, wait_arr_offset]);
        let wait_arr = builder.inst_results(call_wait_arr)[0];

        let aligned_i = builder.ins().imul_imm(i, 8);
        let call_entry = builder.ins().call(load_ref, &[wait_arr, aligned_i]);
        let entry = builder.inst_results(call_entry)[0];

        let is_match = builder.ins().icmp(IntCC::Equal, entry, pid_val);

        let next_scan_block = builder.create_block();
        builder.ins().brif(
            is_match,
            found_block,
            &[BlockArg::Value(i)],
            next_scan_block,
            &[],
        );

        // ── next_scan_block: increment i, check bounds ─────────────────
        builder.switch_to_block(next_scan_block);
        let next_i = builder.ins().iadd_imm(i, 1);

        // Reload last_waited_idx
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);
        let last_idx_offset = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_WAITED_PROCESS_INDEX as i64);
        let call_last = builder.ins().call(load_ref, &[sh_ctx_ptr, last_idx_offset]);
        let last_idx = builder.inst_results(call_last)[0];

        let past_end = builder.ins().icmp(IntCC::SignedGreaterThan, next_i, last_idx);
        builder.ins().brif(
            past_end,
            not_found_block,
            &[],
            scan_block,
            &[BlockArg::Value(next_i)],
        );

        // ── found_block(found_i): swap-remove and add to process_arr ───
        builder.switch_to_block(found_block);
        let found_i = builder.block_params(found_block)[0];
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);

        // Reload wait_arr and last_waited_idx
        let wait_arr_offset = builder.ins().iconst(ptr_ty, Self::WAIT_ARR_FAT as i64);
        let call_wa = builder.ins().call(load_ref, &[sh_ctx_ptr, wait_arr_offset]);
        let wait_arr = builder.inst_results(call_wa)[0];

        let last_idx_offset = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_WAITED_PROCESS_INDEX as i64);
        let call_li = builder.ins().call(load_ref, &[sh_ctx_ptr, last_idx_offset]);
        let last_idx = builder.inst_results(call_li)[0];

        let found_aligned = builder.ins().imul_imm(found_i, 8);
        let last_aligned = builder.ins().imul_imm(last_idx, 8);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        // Copy last → found
        let call_last_entry = builder.ins().call(load_ref, &[wait_arr, last_aligned]);
        let last_entry = builder.inst_results(call_last_entry)[0];
        builder.ins().call(
            store_ref,
            &[wait_arr, last_entry, ptr_size, found_aligned],
        );

        // Zero last slot
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder
            .ins()
            .call(store_ref, &[wait_arr, zero, ptr_size, last_aligned]);

        // Decrement LAST_WAITED_PROCESS_INDEX
        let new_last = builder.ins().iadd_imm(last_idx, -1);
        let last_idx_offset = builder
            .ins()
            .iconst(ptr_ty, Self::LAST_WAITED_PROCESS_INDEX as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_last, ptr_size, last_idx_offset],
        );

        // Decrement WAITED_PROCESS_COUNT
        let count_offset = builder
            .ins()
            .iconst(ptr_ty, Self::WAITED_PROCESS_COUNT as i64);
        let call_wc = builder
            .ins()
            .call(load_ref, &[sh_ctx_ptr, count_offset]);
        let waited_count = builder.inst_results(call_wc)[0];
        let new_wc = builder.ins().iadd_imm(waited_count, -1);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_wc, ptr_size, count_offset],
        );

        // Add process back to active queue
        Self::new_process(sh_ctx_ptr, pid_val, ctx, builder)?;

        builder.ins().jump(after_block, &[]);

        // ── not_found_block: no-op, message is in mailbox ──────────────
        builder.switch_to_block(not_found_block);
        builder.ins().jump(after_block, &[]);

        Ok(())
    }
}
