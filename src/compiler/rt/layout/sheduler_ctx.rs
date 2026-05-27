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

    pub const SIZE: i32 = 224;
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
    /// Generation-tagged pid table.  Each entry is 16 bytes:
    ///   { gen: i64, proc_ctx_fat_ptr: i64 }
    /// PIDs are encoded `(gen << 32) | slot`; lookup compares `entry.gen`
    /// against the high 32 bits of the pid before returning proc_ctx —
    /// stale pids (those carrying the old gen of a recycled slot) fail
    /// the check and resolve to 0 (silent send drop, no false wake).
    /// Slot 0 is reserved as the null-pid sentinel; pid 0 always reads
    /// gen=0, slot=0, where pid_table[0].gen=0 by zero-init.  Real pids
    /// start at gen=1, so the null check always succeeds harmlessly.
    pub const PID_TABLE_FAT: i32 = 176;
    /// Capacity (in slots, 16 bytes each) of `pid_table`.  Doubles when
    /// the high water mark reaches the cap.
    pub const PID_TABLE_CAP: i32 = 184;
    /// High water mark — the next fresh slot to allocate when
    /// `free_slots` is empty.  Starts at 1 (slot 0 reserved for null
    /// sentinel).
    pub const PID_TABLE_LEN: i32 = 192;
    /// Stack of recycled slot indices.  When an actor dies, its slot
    /// index is pushed here; `assign_pid` pops before consuming a fresh
    /// slot from `PID_TABLE_LEN`.  Bounded by peak-concurrent dead-but-
    /// not-yet-reused actors.
    pub const FREE_SLOTS_FAT: i32 = 200;
    pub const FREE_SLOTS_CAP: i32 = 208;
    pub const FREE_SLOTS_LEN: i32 = 216;
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

    /// Starting capacity for `process_arr`, `wait_arr`, `pid_table`, etc.
    /// Doubled each time the high-water mark is reached.  Raising this from
    /// 256 to 4096 amortizes away the grow path on spawn-heavy
    /// microbenchmarks: 100k spawns used to trigger 9 grow events
    /// (256 → 512 → 1024 → … → 65536); with 4096 they trigger 5 and the
    /// hot-loop fast path (`next_index < cap`) hits more consistently.
    /// Memory cost: ~64 KiB extra at startup (4 arrays × 4096 × 8B), which
    /// is well below the per-actor footprint anyway.
    pub const INITIAL_QUEUE_CAP: i64 = 4096;
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
        let allocate_ref = rt_funcs.allocate_raw_ref(ctx.module_mut(), builder);

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

        // pid_table — generation-tagged 16-byte entries: { gen, proc_ctx }.
        // Slot 0 is the null-pid sentinel (zero-init keeps gen=0).
        // PID_TABLE_LEN starts at 1 so the first fresh slot allocation
        // returns slot=1 (pid 0 is reserved).
        let init_bytes = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP * 16);
        let call_pt = builder.ins().call(allocate_ref, &[init_bytes]);
        let pid_table_ptr = builder.inst_results(call_pt)[0];
        let pid_table_offset = builder.ins().iconst(ptr_ty, Self::PID_TABLE_FAT as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, pid_table_ptr, ptr_size, pid_table_offset],
        );

        let init_cap = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP);
        let pid_cap_offset = builder.ins().iconst(ptr_ty, Self::PID_TABLE_CAP as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, init_cap, ptr_size, pid_cap_offset]);

        let one = builder.ins().iconst(ptr_ty, 1);
        let pid_len_offset = builder.ins().iconst(ptr_ty, Self::PID_TABLE_LEN as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, one, ptr_size, pid_len_offset]);

        // free_slots — stack of recycled slot indices (i64 each).
        let init_bytes = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP * 8);
        let call_fs = builder.ins().call(allocate_ref, &[init_bytes]);
        let free_slots_ptr = builder.inst_results(call_fs)[0];
        let free_slots_offset = builder.ins().iconst(ptr_ty, Self::FREE_SLOTS_FAT as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, free_slots_ptr, ptr_size, free_slots_offset],
        );

        let init_cap = builder.ins().iconst(ptr_ty, Self::INITIAL_QUEUE_CAP);
        let free_cap_offset = builder.ins().iconst(ptr_ty, Self::FREE_SLOTS_CAP as i64);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, init_cap, ptr_size, free_cap_offset],
        );
        // FREE_SLOTS_LEN already 0 from zero-init.

        let var = builder.declare_var(ptr_ty);
        builder.def_var(var, sh_ctx_ptr);
        Ok(var)
    }

    /// Mask for the slot half of a packed pid `(gen << 32) | slot`.
    pub const SLOT_MASK: i64 = 0xFFFF_FFFF;

    /// Pop a slot index from `free_slots`, or fall back to a fresh slot
    /// from the high-water mark if the stack is empty.  Internal helper
    /// for [`assign_pid`].
    fn emit_acquire_slot(
        sh_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Value {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let fs_len_off = builder.ins().iconst(ptr_ty, Self::FREE_SLOTS_LEN as i64);
        let call_fs_len = builder.ins().call(load_ref, &[sh_ctx_ptr, fs_len_off]);
        let fs_len = builder.inst_results(call_fs_len)[0];
        let stack_empty = builder.ins().icmp_imm(IntCC::Equal, fs_len, 0);

        let pop_block = builder.create_block();
        let fresh_block = builder.create_block();
        let merge_block = builder.create_block();
        builder.append_block_param(merge_block, ptr_ty);

        builder
            .ins()
            .brif(stack_empty, fresh_block, &[], pop_block, &[]);

        // ── pop_block: slot = free_slots[len - 1]; len -= 1 ──────────────────
        builder.switch_to_block(pop_block);
        builder.seal_block(pop_block);
        let fs_off = builder.ins().iconst(ptr_ty, Self::FREE_SLOTS_FAT as i64);
        let call_fs = builder.ins().call(load_ref, &[sh_ctx_ptr, fs_off]);
        let free_slots = builder.inst_results(call_fs)[0];
        let new_len = builder.ins().iadd_imm(fs_len, -1);
        let aligned = builder.ins().imul_imm(new_len, 8);
        let call_slot = builder.ins().call(load_ref, &[free_slots, aligned]);
        let popped = builder.inst_results(call_slot)[0];
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_len, ptr_size, fs_len_off]);
        builder.ins().jump(merge_block, &[BlockArg::Value(popped)]);

        // ── fresh_block: slot = pid_table_len++ ─────────────────────────────
        builder.switch_to_block(fresh_block);
        builder.seal_block(fresh_block);
        let pt_len_off = builder.ins().iconst(ptr_ty, Self::PID_TABLE_LEN as i64);
        let call_pt_len = builder.ins().call(load_ref, &[sh_ctx_ptr, pt_len_off]);
        let cur_len = builder.inst_results(call_pt_len)[0];
        let next_len = builder.ins().iadd_imm(cur_len, 1);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, next_len, ptr_size, pt_len_off]);
        builder.ins().jump(merge_block, &[BlockArg::Value(cur_len)]);

        builder.switch_to_block(merge_block);
        builder.seal_block(merge_block);
        builder.block_params(merge_block)[0]
    }

    /// Acquire a slot, bump its generation, store `proc_ctx_fat_ptr`,
    /// and return the packed pid `(gen << 32) | slot`.  The first
    /// real pid has gen=1 (slot 0's gen=0 stays reserved as the null
    /// sentinel).
    ///
    /// Generation-tagged design (restored from the original #74
    /// design after the slim variant was found to leak ~566 B/req
    /// in lake-server via monotonic pid_table growth).  Each entry
    /// is 16 bytes `{gen, proc_ctx}` — gen survives slot recycling
    /// so a stale pid resolves to a 0 proc_ctx (silent send drop),
    /// not to a different live actor.
    ///
    /// Allocation order: pop from `free_slots` if non-empty;
    /// otherwise consume a fresh slot from `PID_TABLE_LEN`.  Both
    /// paths increment the slot's gen before writing the new
    /// proc_ctx, ensuring any stale pids carrying the old gen
    /// fail the `lookup_proc_ctx` check.
    pub fn assign_pid(
        sh_ctx_ptr: Value,
        proc_ctx_fat_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Value {
        let ptr_ty = ctx.module().target_config().pointer_type();

        // Acquire slot — pop from free_slots if any, else bump LEN.
        let slot = Self::emit_acquire_slot(sh_ctx_ptr, ctx, builder);

        // Inline load: sh_ctx_ptr is a fat-ptr addr; deref once.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);

        // Grow pid_table so the entry at `slot` fits (stride 16 = log_stride 4).
        // Pass `slot + 1` as the high-water-mark — emit_grow_array_if_full_strided
        // doubles when `mark >= cap`.
        let mark = builder.ins().iadd_imm(slot, 1);
        let pid_table_fat = Self::emit_grow_array_if_full_strided(
            sh_ctx_ptr,
            Self::PID_TABLE_FAT,
            Self::PID_TABLE_CAP,
            mark,
            4,
            ctx,
            builder,
        );
        let pid_table = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), pid_table_fat, 0);

        // entry_addr = pid_table[slot * 16]
        let entry_off = builder.ins().imul_imm(slot, 16);
        let entry_addr = builder.ins().iadd(pid_table, entry_off);

        // Bump the slot's generation: new_gen = old_gen + 1.
        let old_gen = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), entry_addr, 0);
        let new_gen = builder.ins().iadd_imm(old_gen, 1);
        builder
            .ins()
            .store(MemFlags::trusted(), new_gen, entry_addr, 0);
        // Write the new proc_ctx at offset 8.
        builder
            .ins()
            .store(MemFlags::trusted(), proc_ctx_fat_ptr, entry_addr, 8);

        // Pack pid = (new_gen << 32) | slot.
        let _ = sh_data;
        let gen_shifted = builder.ins().ishl_imm(new_gen, 32);
        builder.ins().bor(gen_shifted, slot)
    }

    /// Look up `proc_ctx_fat_ptr` for a given pid, or return 0 if the pid
    /// is dead / null / out of range.  The generation half of the pid
    /// is compared against `pid_table[slot].gen` — a mismatch (recycled
    /// slot, stale pid from a dead actor) yields 0 so sends silently
    /// drop instead of waking the wrong actor (#73).
    pub fn lookup_proc_ctx(
        sh_ctx_ptr: Value,
        pid: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Value {
        let ptr_ty = ctx.module().target_config().pointer_type();

        // Inline load chain.  pid_table_fat lives in trusted sh_ctx memory.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);
        let pid_table_fat =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sh_data, Self::PID_TABLE_FAT);
        let pid_table = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), pid_table_fat, 0);

        // Unpack pid → (gen, slot).
        let slot = builder.ins().band_imm(pid, Self::SLOT_MASK);
        let pid_gen = builder.ins().ushr_imm(pid, 32);

        let entry_off = builder.ins().imul_imm(slot, 16);
        let entry_addr = builder.ins().iadd(pid_table, entry_off);

        let stored_gen = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), entry_addr, 0);
        let proc_ctx = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), entry_addr, 8);

        // gen mismatch → return 0 (stale pid, slot was recycled).
        let gen_ok = builder.ins().icmp(IntCC::Equal, stored_gen, pid_gen);
        let zero = builder.ins().iconst(ptr_ty, 0);
        let result = builder.ins().select(gen_ok, proc_ctx, zero);
        let _ = ctx;
        result
    }

    /// Mark `pid` as dead and push its slot onto `free_slots` for reuse.
    /// The slot's gen counter is left unchanged so subsequent
    /// `lookup_proc_ctx` with the now-stale pid still fails the gen
    /// check (`assign_pid` bumps gen on next reuse).  Slot 0 is the
    /// null sentinel — never recycled.
    pub fn clear_pid(
        sh_ctx_ptr: Value,
        pid: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        let ptr_size = builder.ins().iconst(ptr_ty, ptr_ty.bytes() as i64);

        let slot = builder.ins().band_imm(pid, Self::SLOT_MASK);

        // Inline load chain.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);
        let pid_table_fat =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sh_data, Self::PID_TABLE_FAT);
        let pid_table = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), pid_table_fat, 0);

        // entry = pid_table[slot * 16]; zero proc_ctx (offset 8), keep gen.
        let entry_off = builder.ins().imul_imm(slot, 16);
        let entry_addr = builder.ins().iadd(pid_table, entry_off);
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), zero, entry_addr, 8);

        // Slot 0 = null sentinel; never push back.
        let is_real = builder.ins().icmp_imm(IntCC::NotEqual, slot, 0);
        let push_block = builder.create_block();
        let done_block = builder.create_block();
        builder
            .ins()
            .brif(is_real, push_block, &[], done_block, &[]);

        // Push slot onto free_slots stack — grow if at cap.
        builder.switch_to_block(push_block);
        builder.seal_block(push_block);
        let fs_len_off = builder.ins().iconst(ptr_ty, Self::FREE_SLOTS_LEN as i64);
        let call_fs_len = builder.ins().call(load_ref, &[sh_ctx_ptr, fs_len_off]);
        let fs_len = builder.inst_results(call_fs_len)[0];
        let new_fs_len = builder.ins().iadd_imm(fs_len, 1);
        let free_slots_fat = Self::emit_grow_array_if_full_strided(
            sh_ctx_ptr,
            Self::FREE_SLOTS_FAT,
            Self::FREE_SLOTS_CAP,
            new_fs_len,
            3,
            ctx,
            builder,
        );
        let free_slots = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), free_slots_fat, 0);
        let push_off = builder.ins().imul_imm(fs_len, 8);
        let push_addr = builder.ins().iadd(free_slots, push_off);
        builder
            .ins()
            .store(MemFlags::trusted(), slot, push_addr, 0);
        builder.ins().call(
            store_ref,
            &[sh_ctx_ptr, new_fs_len, ptr_size, fs_len_off],
        );
        builder.ins().jump(done_block, &[]);

        builder.switch_to_block(done_block);
        builder.seal_block(done_block);
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
        let allocate_ref = rt_funcs.allocate_raw_ref(ctx.module_mut(), builder);

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
        // Death flag — see ExecCtxLayout::IS_DYING.
        ExecCtxLayout::store(builder, zero, main_ctx_ptr, ExecCtxLayout::IS_DYING);

        let process_ctx = ProcessCtxLayout::init_ctx(ctx, builder, "main", main_ctx_fat_ptr)?;

        // Give main its own arena (#138).  Main is the root actor —
        // arena reclaimed at process exit by the OS, so it's
        // effectively a global heap.  Without an arena, every
        // tuple/record literal and every ret-machine-helper alloc
        // (concat / slice / int_to_buf, …) made by main would fall
        // back to rt_allocate_raw and accumulate as leaks for the
        // process lifetime.
        const MAIN_ARENA_BYTES: i64 = 64 * 1024;
        let allocate_raw_ref = ctx
            .rt_funcs()
            .clone()
            .allocate_raw_ref(ctx.module_mut(), builder);
        let main_arena_size = builder.ins().iconst(ptr_ty, MAIN_ARENA_BYTES);
        let call_main_arena = builder.ins().call(allocate_raw_ref, &[main_arena_size]);
        let main_arena_fat = builder.inst_results(call_main_arena)[0];
        let main_arena_base = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), main_arena_fat, 0);
        let main_store_ref = ctx.rt_funcs().clone().store_ref(ctx.module_mut(), builder);
        let main_field_size = builder.ins().iconst(ptr_ty, 8);
        let main_arena_fat_off = builder
            .ins()
            .iconst(ptr_ty, ProcessCtxLayout::OWNED_ARENA_FAT as i64);
        builder.ins().call(
            main_store_ref,
            &[process_ctx, main_arena_fat, main_field_size, main_arena_fat_off],
        );
        let main_arena_base_off = builder
            .ins()
            .iconst(ptr_ty, ProcessCtxLayout::OWNED_ARENA_BASE as i64);
        builder.ins().call(
            main_store_ref,
            &[process_ctx, main_arena_base, main_field_size, main_arena_base_off],
        );

        // Assign monotonic pid to main and stash it in OWN_PID.  Source-
        // level `self` reads return this pid; sends look it up in the
        // pid_table to find main's proc_ctx.
        let sh_ctx_ptr_for_assign = builder.use_var(sh_ptr_var);
        let main_pid = Self::assign_pid(sh_ctx_ptr_for_assign, process_ctx, ctx, builder);
        ExecCtxLayout::store(builder, main_pid, main_ctx_ptr, ExecCtxLayout::OWN_PID);

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
        // Inlined: skip the rt_load_u64 function call.  Scheduler ctx is a
        // trusted, well-known global — no bounds check needed.  Two direct
        // loads (fat-ptr deref + field load) where there used to be a call,
        // saves ~5 ns / scheduler tick on every dispatch.
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);
        let real_count_of_processes = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
        );
        let _ = ctx;
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
        let exec_ctx_offset = builder
            .ins()
            .iconst(ptr_ty, ProcessCtxLayout::EXEC_CTX as i64);
        let call_exec = builder
            .ins()
            .call(load_ref, &[process_ctx_fat_ptr, exec_ctx_offset]);
        let exec_ctx_fat_ptr = builder.inst_results(call_exec)[0];

        // Read OWN_PID before any free so we can clear pid_table[pid]
        // and prevent stale pids from resolving to a recycled proc_ctx
        // (root cause of #73).
        let own_pid_off = builder.ins().iconst(ptr_ty, ExecCtxLayout::OWN_PID as i64);
        let call_pid = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, own_pid_off]);
        let dead_pid = builder.inst_results(call_pid)[0];

        // Read the three nested fat-ptrs from the ExecCtx payload before any
        // free, since freeing them clobbers their start fields.
        let vars_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::VARIABLES as i64);
        let call_vars = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, vars_offset]);
        let vars_fat_ptr = builder.inst_results(call_vars)[0];

        let args_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::JUMP_ARGS as i64);
        let call_args = builder
            .ins()
            .call(load_ref, &[exec_ctx_fat_ptr, args_offset]);
        let args_fat_ptr = builder.inst_results(call_args)[0];

        let mb_offset = builder
            .ins()
            .iconst(ptr_ty, ExecCtxLayout::MAILBOX_FAT as i64);
        let call_mb = builder.ins().call(load_ref, &[exec_ctx_fat_ptr, mb_offset]);
        let mailbox_fat_ptr = builder.inst_results(call_mb)[0];

        // Mark the pid as dead in the table.  Future sends to this pid
        // resolve to a 0 proc_ctx and silently drop, even after the
        // free-list recycles the proc_ctx address into a new actor.
        let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
            Some(cranelift::module::FuncOrDataId::Data(id)) => Some(id),
            _ => None,
        };
        if let Some(id) = sched_data_id {
            let sched_gv = ctx.module_mut().declare_data_in_func(id, &mut builder.func);
            let sh_ctx_ptr = builder.ins().global_value(ptr_ty, sched_gv);
            Self::clear_pid(sh_ctx_ptr, dead_pid, ctx, builder);
        }

        // ── Reclaim the actor's arena (#138).  Only the owning actor
        // frees — inherited arenas (sync ret-machine spawns share the
        // caller's arena, phase 2c) are owned by the caller and left
        // alone here.  OWNED_ARENA_BASE != 0 marks ownership.
        let arena_fat_off = builder
            .ins()
            .iconst(ptr_ty, ProcessCtxLayout::OWNED_ARENA_FAT as i64);
        let arena_base_off = builder
            .ins()
            .iconst(ptr_ty, ProcessCtxLayout::OWNED_ARENA_BASE as i64);
        let call_arena_fat = builder
            .ins()
            .call(load_ref, &[process_ctx_fat_ptr, arena_fat_off]);
        let arena_fat_ptr = builder.inst_results(call_arena_fat)[0];
        let call_arena_base = builder
            .ins()
            .call(load_ref, &[process_ctx_fat_ptr, arena_base_off]);
        let arena_base = builder.inst_results(call_arena_base)[0];

        let arena_owned_block = builder.create_block();
        let frees_block = builder.create_block();
        let owns_arena = builder.ins().icmp_imm(IntCC::NotEqual, arena_base, 0);
        builder
            .ins()
            .brif(owns_arena, arena_owned_block, &[], frees_block, &[]);

        builder.switch_to_block(arena_owned_block);
        builder.seal_block(arena_owned_block);
        // Restore the fat-ptr's start to the original base — bump
        // mutations made `start` point mid-arena.  rt_free reads
        // start/end to compute the bucket size; with mid-arena start
        // it would mis-classify.
        builder
            .ins()
            .store(MemFlags::trusted(), arena_base, arena_fat_ptr, 0);
        builder.ins().call(free_ref, &[arena_fat_ptr]);
        builder.ins().jump(frees_block, &[]);

        builder.switch_to_block(frees_block);
        builder.seal_block(frees_block);

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
        // Inlined: three rt_load_u64 calls → three direct loads.  Saves
        // ~15 ns / scheduler tick (3 function-call frames eliminated).
        let sh_ctx_ptr = builder.use_var(sh_ctx_ptr);
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);
        let current_process_index = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            ShedulerCtxLayout::CURRENT_PROCESS,
        );
        let aligned_index = builder.ins().imul_imm(current_process_index, 8);

        let process_arr_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            ShedulerCtxLayout::PROCESS_ARR_FAT,
        );
        let process_arr_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), process_arr_fat, 0);
        let entry_addr = builder.ins().iadd(process_arr_start, aligned_index);
        let current_process = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), entry_addr, 0);

        let _ = ctx;
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
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);
        // Inlined fat-ptr deref + field accesses.  Each rt_load_u64 / rt_store
        // call replaced with a direct load / store.  Called per actor death,
        // common on msg/spawn microbenches.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);

        let current_idx =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sh_data, Self::CURRENT_PROCESS);
        let current_aligned = builder.ins().imul_imm(current_idx, 8);

        let last_idx = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            Self::LAST_PROCESS_INDEX,
        );
        let last_aligned = builder.ins().imul_imm(last_idx, 8);

        let process_arr_fat =
            builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), sh_data, Self::PROCESS_ARR_FAT);
        let process_arr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), process_arr_fat, 0);

        // Swap-and-pop: copy last → current, zero last.
        let last_proc_addr = builder.ins().iadd(process_arr, last_aligned);
        let last_proc = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), last_proc_addr, 0);
        let current_proc_addr = builder.ins().iadd(process_arr, current_aligned);
        builder
            .ins()
            .store(MemFlags::trusted(), last_proc, current_proc_addr, 0);
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), zero, last_proc_addr, 0);

        // Shrink the array.
        let new_last = builder.ins().iadd_imm(last_idx, -1);
        builder.ins().store(
            MemFlags::trusted(),
            new_last,
            sh_data,
            Self::LAST_PROCESS_INDEX,
        );

        // Decrement active count.
        let real_count = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            Self::REAL_COUNT_OF_PROCESSES,
        );
        let new_count = builder.ins().iadd_imm(real_count, -1);
        builder.ins().store(
            MemFlags::trusted(),
            new_count,
            sh_data,
            Self::REAL_COUNT_OF_PROCESSES,
        );

        // Fix CURRENT_PROCESS if we just removed the last slot.
        let reset_block = builder.create_block();
        let done_block = builder.create_block();

        let was_last = builder.ins().icmp(IntCC::Equal, current_idx, last_idx);
        builder
            .ins()
            .brif(was_last, reset_block, &[], done_block, &[]);

        builder.switch_to_block(reset_block);
        // Re-load sh_data in case scheduler ctx moved; defensive though
        // currently sheduler_ctx_fat_ptr is stable.
        let sh_ctx_ptr2 = builder.use_var(sh_ptr_var);
        let sh_data2 = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr2, 0);
        let zero = builder.ins().iconst(ptr_ty, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), zero, sh_data2, Self::CURRENT_PROCESS);
        builder.ins().jump(loop_block, &[]);

        builder.switch_to_block(done_block);
        builder.ins().jump(loop_block, &[]);

        let _ = ctx;
        Ok(())
    }

    pub fn wait_current_process(
        sh_ptr_var: Variable,
        process_ctx_ptr: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Result<()> {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let sh_ctx_ptr = builder.use_var(sh_ptr_var);
        // Inlined fat-ptr deref chain.  Called per `wait` yield —
        // ping_pong hits this 200k times.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);

        // Resolve pid from proc_ctx.EXEC_CTX.OWN_PID — wait_arr stores
        // pids (i64) so wake_process can match on the pid the sender
        // passed, not on a recyclable proc_ctx address.
        //
        // process_ctx_ptr is a FAT-PTR ADDRESS, not the raw proc_ctx
        // start.  Deref it first to get the actual ProcessCtx layout
        // bytes.  Same pattern for exec_ctx_fat → exec_start.
        let proc_ctx_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), process_ctx_ptr, 0);
        let exec_ctx_fat = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            proc_ctx_start,
            ProcessCtxLayout::EXEC_CTX,
        );
        let exec_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), exec_ctx_fat, 0);
        let pid = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::OWN_PID,
        );

        let last_process_index = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            Self::LAST_WAITED_PROCESS_INDEX,
        );
        let next_process_index = builder.ins().iadd_imm(last_process_index, 1);
        let aligned_index = builder.ins().imul_imm(next_process_index, 8);

        // Grow wait_arr if next_index would exceed cap; use the (possibly
        // new) fat-ptr returned for the store below.
        let wait_arr_fat = Self::emit_grow_array_if_full(
            sh_ctx_ptr,
            Self::WAIT_ARR_FAT,
            Self::WAIT_ARR_CAP,
            next_process_index,
            ctx,
            builder,
        );
        let wait_arr_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), wait_arr_fat, 0);
        let slot_addr = builder.ins().iadd(wait_arr_start, aligned_index);
        builder.ins().store(MemFlags::trusted(), pid, slot_addr, 0);

        builder.ins().store(
            MemFlags::trusted(),
            next_process_index,
            sh_data,
            Self::LAST_WAITED_PROCESS_INDEX,
        );

        // Increment WAITED_PROCESS_COUNT
        let waited_count = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            Self::WAITED_PROCESS_COUNT,
        );
        let new_count = builder.ins().iadd_imm(waited_count, 1);
        builder.ins().store(
            MemFlags::trusted(),
            new_count,
            sh_data,
            Self::WAITED_PROCESS_COUNT,
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
        // Default stride 8 bytes per slot — what process_arr / wait_arr /
        // free_slots all use.  pid_table needs 16 → call the strided
        // variant directly.
        Self::emit_grow_array_if_full_strided(
            sh_ctx_ptr, fat_offset, cap_offset, next_index, 3, ctx, builder,
        )
    }

    /// Same growth contract as [`emit_grow_array_if_full`] but with a
    /// caller-supplied `log_stride` so 16-byte (`log_stride=4`) entries
    /// like `pid_table` get the right byte count when allocating /
    /// copying.
    fn emit_grow_array_if_full_strided(
        sh_ctx_ptr: Value,
        fat_offset: i32,
        cap_offset: i32,
        next_index: Value,
        log_stride: i64,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) -> Value {
        let ptr_ty = ctx.module().target_config().pointer_type();
        let rt_funcs = ctx.rt_funcs().clone();
        let load_ref = rt_funcs.load_u64_ref(ctx.module_mut(), builder);
        let store_ref = rt_funcs.store_ref(ctx.module_mut(), builder);
        // Growth path: copies old entries into new buffer immediately, so
        // zero-init on free-list pop would be wasted bandwidth.
        let allocate_ref = rt_funcs.allocate_raw_ref(ctx.module_mut(), builder);
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
        builder.ins().jump(merge_block, &[BlockArg::Value(old_fat)]);

        // ── grow_block: alloc 2× → copy → free → update fields ──────────────
        builder.switch_to_block(grow_block);
        builder.seal_block(grow_block);

        let new_cap = builder.ins().ishl_imm(cap, 1);
        let alloc_bytes = builder.ins().ishl_imm(new_cap, log_stride);
        let call_alloc = builder.ins().call(allocate_ref, &[alloc_bytes]);
        let new_fat = builder.inst_results(call_alloc)[0];

        // Copy `next_index` entries from old to new.  We copy in 8-byte
        // chunks regardless of stride because both 8B and 16B slots
        // align to 8 — `chunks = next_index << (log_stride - 3)`.
        let old_start = builder.ins().load(ptr_ty, MemFlags::trusted(), old_fat, 0);
        let new_start = builder.ins().load(ptr_ty, MemFlags::trusted(), new_fat, 0);
        let chunks = builder.ins().ishl_imm(next_index, log_stride - 3);

        let copy_header = builder.create_block();
        let copy_body = builder.create_block();
        let copy_done = builder.create_block();
        builder.append_block_param(copy_header, ptr_ty);

        let zero = builder.ins().iconst(ptr_ty, 0);
        builder.ins().jump(copy_header, &[BlockArg::Value(zero)]);

        builder.switch_to_block(copy_header);
        let i = builder.block_params(copy_header)[0];
        let cmp = builder.ins().icmp(IntCC::UnsignedLessThan, i, chunks);
        builder.ins().brif(cmp, copy_body, &[], copy_done, &[]);

        builder.switch_to_block(copy_body);
        builder.seal_block(copy_body);
        let off = builder.ins().ishl_imm(i, 3);
        let src_addr = builder.ins().iadd(old_start, off);
        let dst_addr = builder.ins().iadd(new_start, off);
        let val = builder.ins().load(ptr_ty, MemFlags::trusted(), src_addr, 0);
        builder.ins().store(MemFlags::trusted(), val, dst_addr, 0);
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

        builder.ins().jump(merge_block, &[BlockArg::Value(new_fat)]);

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
        let store_ref = rt_func.store_ref(ctx.module_mut(), builder);
        let sh_ctx_ptr = builder.use_var(sh_ctx_var);
        // Inlined load chain: scheduler ctx is well-known trusted memory.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);

        let current_process_index = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            ShedulerCtxLayout::CURRENT_PROCESS,
        );

        let last_process_index = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            ShedulerCtxLayout::LAST_PROCESS_INDEX,
        );

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
        // Inlined fat-ptr deref: scheduler ctx is well-known trusted memory.
        // Each rt_load_u64 call replaced with two direct loads (fat-ptr deref
        // + field load) saves a function-call frame per send.  Ping_pong with
        // 200k sends amortizes the savings into ~3-5 ms.
        let sh_data = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_ctx_ptr, 0);

        let wait_arr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), sh_data, Self::WAIT_ARR_FAT);

        let last_waited_idx = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            sh_data,
            Self::LAST_WAITED_PROCESS_INDEX,
        );

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

        // wait_arr was defined in the entry block before the brif —
        // SSA values are visible in dominated successors so the reload
        // is redundant.  Inline the wait_arr fat-ptr deref + indexed
        // load to skip two rt_load_u64 function calls per scan step.
        let wait_arr_start = builder.ins().load(ptr_ty, MemFlags::trusted(), wait_arr, 0);
        let aligned_i = builder.ins().imul_imm(i, 8);
        let entry_addr = builder.ins().iadd(wait_arr_start, aligned_i);
        let entry = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), entry_addr, 0);

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

        // last_waited_idx defined in entry block — SSA visible here.
        let past_end = builder
            .ins()
            .icmp(IntCC::SignedGreaterThan, next_i, last_waited_idx);
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
        builder
            .ins()
            .call(store_ref, &[wait_arr, last_entry, ptr_size, found_aligned]);

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
        let call_wc = builder.ins().call(load_ref, &[sh_ctx_ptr, count_offset]);
        let waited_count = builder.inst_results(call_wc)[0];
        let new_wc = builder.ins().iadd_imm(waited_count, -1);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_ptr, new_wc, ptr_size, count_offset]);

        // pid_val is a monotonic pid; look up the live proc_ctx fat-ptr
        // from the pid_table to enqueue into process_arr.  If the pid
        // was already cleared (dead actor), the lookup returns 0 and we
        // skip the enqueue — handled by the brif below.
        let proc_ctx = Self::lookup_proc_ctx(sh_ctx_ptr, pid_val, ctx, builder);
        let lookup_ok = builder.ins().icmp_imm(IntCC::NotEqual, proc_ctx, 0);
        let enqueue_block = builder.create_block();
        builder
            .ins()
            .brif(lookup_ok, enqueue_block, &[], after_block, &[]);
        builder.switch_to_block(enqueue_block);
        builder.seal_block(enqueue_block);
        Self::new_process(sh_ctx_ptr, proc_ctx, ctx, builder)?;
        builder.ins().jump(after_block, &[]);

        // ── not_found_block: no-op, message is in mailbox ──────────────
        builder.switch_to_block(not_found_block);
        builder.ins().jump(after_block, &[]);

        Ok(())
    }
}
