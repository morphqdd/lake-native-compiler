use anyhow::Result;
use cranelift::{
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, Value},
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::ExecCtxLayout};

pub struct ProcessCtxLayout;

impl ProcessCtxLayout {
    pub const FUNC_PTR: i32 = 0;
    pub const EXEC_CTX: i32 = 8;
    /// Slot index in `sh_ctx.io_parked` while this actor is parked, or
    /// undefined when the actor is in `process_arr`.  Maintained by
    /// `rt_io_park_current` (write on park) and `emit_wake_by_user_data`
    /// (used to swap-and-pop in O(1) without scanning `io_parked`).
    pub const IO_PARKED_IDX: i32 = 16;
    /// Per-actor arena fat-ptr (feature #138).  The fat-ptr's `start`
    /// field doubles as the bump cursor — `rt_arena_alloc` reads it,
    /// advances it, writes it back.  The `end` field is the arena
    /// boundary, set once at spawn.
    ///
    /// For inherited arenas (sync ret-machine spawns share the
    /// caller's arena — see #138 phase 2c), this field holds the
    /// SAME pointer as the caller's, so a child's bump automatically
    /// propagates back to the parent.  No writeback step needed —
    /// it's literally the same memory.
    pub const OWNED_ARENA_FAT: i32 = 24;
    /// Original arena base address (= `*OWNED_ARENA_FAT` at spawn
    /// time, before any bumps).  Non-zero means "this actor owns the
    /// arena" — `free_process_resources` restores the fat-ptr's
    /// `start` to this value and calls `rt_free`.  Zero means
    /// "inherited from caller, don't free".
    pub const OWNED_ARENA_BASE: i32 = 32;
    pub const SIZE: i32 = 40;

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
        // ProcessCtx fields (func_ptr, exec_ctx, io_parked_idx) are
        // explicitly written below — no need for zero-init.
        let allocate_ref = rt_funcs.allocate_raw_ref(ctx.module_mut(), builder);
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

        // Explicitly clear IO_PARKED_IDX — would normally be zero-init'd by
        // rt_allocate's free-list pop, but `_raw` skips that.  The io_uring
        // park path reads this field on unpark routing, so a stale value
        // (e.g. from a previously-died actor that was parked) would
        // mis-route the wake-up.
        let zero = builder.ins().iconst(ptr_ty, 0);
        let io_idx_offset = builder.ins().iconst(ptr_ty, Self::IO_PARKED_IDX as i64);
        builder
            .ins()
            .call(store_ref, &[process_ctx_ptr, zero, ptr_size, io_idx_offset]);

        // Initialise arena fields to 0.  `spawn_expr` overwrites
        // them — either with this actor's own arena fat-ptr (when
        // spawning a non-ret machine) or with the caller's arena
        // fat-ptr (when spawning a sync ret-machine, sharing the
        // caller's allocator).  `rt_arena_alloc` treats `arena_fat
        // == 0` as "no arena, fall back to rt_allocate_raw".
        let arena_off = builder.ins().iconst(ptr_ty, Self::OWNED_ARENA_FAT as i64);
        builder
            .ins()
            .call(store_ref, &[process_ctx_ptr, zero, ptr_size, arena_off]);
        let base_off = builder.ins().iconst(ptr_ty, Self::OWNED_ARENA_BASE as i64);
        builder
            .ins()
            .call(store_ref, &[process_ctx_ptr, zero, ptr_size, base_off]);

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
