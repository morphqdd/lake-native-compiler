//! io_uring runtime — async I/O backend.
//!
//! Stage 2-3: ring setup + `rt_write_async` submit path.  Setup resolves
//! every offset returned by the kernel into an absolute pointer and stores
//! those in the scheduler context, so submit / poll never has to add an
//! offset at runtime.
//!
//! Full design: `docs/io_uring_design.md`.

use anyhow::{Result, anyhow};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags,
        TrapCode, types,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{FatPtrLayout, sheduler_ctx::ShedulerCtxLayout},
};

/// Stop code returned by an actor when it parks on an I/O submission.  The
/// scheduler must NOT call `remove_current_process` for this code — the slot
/// has already been vacated by `io_park_current_actor`.
#[allow(dead_code)]
pub const STOP_PARK: i64 = -3;

// Kernel ABI constants.
const SYS_MMAP: i64 = 9;
const SYS_IO_URING_SETUP: i64 = 425;
const SYS_IO_URING_ENTER: i64 = 426;

const PROT_READ: i64 = 0x1;
const PROT_WRITE: i64 = 0x2;
const MAP_SHARED: i64 = 0x01;
const MAP_POPULATE: i64 = 0x8000;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

const IORING_OP_WRITE: i64 = 23;

// io_uring_params layout (120 bytes total).
const PARAMS_SIZE: i64 = 120;
const PARAMS_OFF_SQ_ENTRIES: i32 = 0;
const PARAMS_OFF_CQ_ENTRIES: i32 = 4;
const PARAMS_OFF_SQ_OFF: i32 = 40;
const PARAMS_OFF_CQ_OFF: i32 = 80;

// io_sqring_offsets / io_cqring_offsets are stable: fields {head, tail,
// ring_mask, ring_entries, ...} live at offsets 0/4/8/12 from the offsets
// struct.  See <linux/io_uring.h>.
const SQ_OFF_HEAD: i32 = 0;
const SQ_OFF_TAIL: i32 = 4;
const SQ_OFF_RING_MASK: i32 = 8;
const SQ_OFF_ARRAY: i32 = 24;
const CQ_OFF_HEAD: i32 = 0;
const CQ_OFF_TAIL: i32 = 4;
const CQ_OFF_RING_MASK: i32 = 8;
const CQ_OFF_CQES: i32 = 20;

const SQE_BYTES: i64 = 64;
const CQE_BYTES: i64 = 16;

const RING_ENTRIES: i64 = 256;
/// Number of SQEs to accumulate before issuing a single `io_uring_enter`.
/// Higher = better throughput, worse latency for the laggard submission.
/// 16 keeps p99 latency well under a millisecond at sane CPU speeds while
/// amortising the syscall cost across 16× more writes.
const SQE_BATCH_SIZE: i64 = 16;

/// Build `rt_io_uring_setup(sh_ctx_fat_ptr)` — performs the full
/// setup-and-stash sequence.
pub fn define_io_uring_setup(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_io_uring_setup")),
    };
    let allocate_id = match ctx.module().get_name("rt_allocate") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_allocate must be declared before rt_io_uring_setup")),
    };
    let free_id = match ctx.module().get_name("rt_free") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_free must be declared before rt_io_uring_setup")),
    };
    let store_id = match ctx.module().get_name("rt_store") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_store must be declared before rt_io_uring_setup")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let sh_ctx_fat = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    let allocate_ref = ctx
        .module_mut()
        .declare_func_in_func(allocate_id, &mut builder.func);
    let free_ref = ctx
        .module_mut()
        .declare_func_in_func(free_id, &mut builder.func);
    let store_ref = ctx
        .module_mut()
        .declare_func_in_func(store_id, &mut builder.func);

    let zero64 = builder.ins().iconst(ty, 0);
    let ptr_size = builder.ins().iconst(ty, 8);

    // ── 1. Allocate params struct, get raw payload ptr ───────────────────────
    let params_size = builder.ins().iconst(ty, PARAMS_SIZE);
    let call_alloc = builder.ins().call(allocate_ref, &[params_size]);
    let params_fat = builder.inst_results(call_alloc)[0];
    let params_start = builder.ins().load(ty, MemFlags::trusted(), params_fat, 0);

    // ── 2. Zero-init params ──────────────────────────────────────────────────
    for i in 0..(PARAMS_SIZE / 8) {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, params_start, (i * 8) as i32);
    }

    // ── 3. io_uring_setup(RING_ENTRIES, params) ─────────────────────────────
    let nr = builder.ins().iconst(ty, SYS_IO_URING_SETUP);
    let entries = builder.ins().iconst(ty, RING_ENTRIES);
    let call_setup = builder.ins().call(
        syscall_ref,
        &[nr, entries, params_start, zero64, zero64, zero64, zero64],
    );
    let fd = builder.inst_results(call_setup)[0];
    let fd_ok = builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, fd, 0);
    builder.ins().trapz(fd_ok, TrapCode::unwrap_user(40));

    // ── 4. Read sizes + offsets from params ──────────────────────────────────
    let load_u32 = |b: &mut FunctionBuilder, off: i32| -> cranelift::prelude::Value {
        b.ins().load(types::I32, MemFlags::trusted(), params_start, off)
    };
    let sq_entries = load_u32(&mut builder, PARAMS_OFF_SQ_ENTRIES);
    let cq_entries = load_u32(&mut builder, PARAMS_OFF_CQ_ENTRIES);

    let sq_off_head = load_u32(&mut builder, PARAMS_OFF_SQ_OFF + SQ_OFF_HEAD);
    let sq_off_tail = load_u32(&mut builder, PARAMS_OFF_SQ_OFF + SQ_OFF_TAIL);
    let sq_off_mask = load_u32(&mut builder, PARAMS_OFF_SQ_OFF + SQ_OFF_RING_MASK);
    let sq_off_array = load_u32(&mut builder, PARAMS_OFF_SQ_OFF + SQ_OFF_ARRAY);
    let cq_off_head = load_u32(&mut builder, PARAMS_OFF_CQ_OFF + CQ_OFF_HEAD);
    let cq_off_tail = load_u32(&mut builder, PARAMS_OFF_CQ_OFF + CQ_OFF_TAIL);
    let cq_off_mask = load_u32(&mut builder, PARAMS_OFF_CQ_OFF + CQ_OFF_RING_MASK);
    let cq_off_cqes = load_u32(&mut builder, PARAMS_OFF_CQ_OFF + CQ_OFF_CQES);
    let _ = sq_off_head;

    let sq_entries_ext = builder.ins().uextend(ty, sq_entries);
    let cq_entries_ext = builder.ins().uextend(ty, cq_entries);
    let sq_off_tail_ext = builder.ins().uextend(ty, sq_off_tail);
    let sq_off_mask_ext = builder.ins().uextend(ty, sq_off_mask);
    let sq_off_array_ext = builder.ins().uextend(ty, sq_off_array);
    let cq_off_head_ext = builder.ins().uextend(ty, cq_off_head);
    let cq_off_tail_ext = builder.ins().uextend(ty, cq_off_tail);
    let cq_off_mask_ext = builder.ins().uextend(ty, cq_off_mask);
    let cq_off_cqes_ext = builder.ins().uextend(ty, cq_off_cqes);

    // ── 5. mmap three regions ────────────────────────────────────────────────
    let nr_mmap = builder.ins().iconst(ty, SYS_MMAP);
    let prot = builder.ins().iconst(ty, PROT_READ | PROT_WRITE);
    let flags_mmap = builder.ins().iconst(ty, MAP_SHARED | MAP_POPULATE);

    // SQ ring length = sq_off.array + sq_entries * 4
    let sq_ring_len = {
        let entries_x4 = builder.ins().ishl_imm(sq_entries_ext, 2);
        builder.ins().iadd(sq_off_array_ext, entries_x4)
    };
    let off_sq = builder.ins().iconst(ty, IORING_OFF_SQ_RING);
    let call_sq = builder.ins().call(
        syscall_ref,
        &[nr_mmap, zero64, sq_ring_len, prot, flags_mmap, fd, off_sq],
    );
    let sq_ring_ptr = builder.inst_results(call_sq)[0];

    // CQ ring length = cq_off.cqes + cq_entries * 16
    let cq_ring_len = {
        let entries_x16 = builder.ins().imul_imm(cq_entries_ext, CQE_BYTES);
        builder.ins().iadd(cq_off_cqes_ext, entries_x16)
    };
    let off_cq = builder.ins().iconst(ty, IORING_OFF_CQ_RING);
    let call_cq = builder.ins().call(
        syscall_ref,
        &[nr_mmap, zero64, cq_ring_len, prot, flags_mmap, fd, off_cq],
    );
    let cq_ring_ptr = builder.inst_results(call_cq)[0];

    let sqe_array_len = builder.ins().imul_imm(sq_entries_ext, SQE_BYTES);
    let off_sqes = builder.ins().iconst(ty, IORING_OFF_SQES);
    let call_sqes = builder.ins().call(
        syscall_ref,
        &[nr_mmap, zero64, sqe_array_len, prot, flags_mmap, fd, off_sqes],
    );
    let sqe_array_ptr = builder.inst_results(call_sqes)[0];

    for ptr in [sq_ring_ptr, cq_ring_ptr, sqe_array_ptr] {
        let ok = builder
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, ptr, 4096);
        builder.ins().trapz(ok, TrapCode::unwrap_user(41));
    }

    // ── 6. Resolve all offsets to absolute pointers ──────────────────────────
    let sq_tail_ptr = builder.ins().iadd(sq_ring_ptr, sq_off_tail_ext);
    let sq_mask_ptr = builder.ins().iadd(sq_ring_ptr, sq_off_mask_ext);
    let sq_array_ptr = builder.ins().iadd(sq_ring_ptr, sq_off_array_ext);
    let cq_head_ptr = builder.ins().iadd(cq_ring_ptr, cq_off_head_ext);
    let cq_tail_ptr = builder.ins().iadd(cq_ring_ptr, cq_off_tail_ext);
    let cq_mask_ptr = builder.ins().iadd(cq_ring_ptr, cq_off_mask_ext);
    let cq_cqes_ptr = builder.ins().iadd(cq_ring_ptr, cq_off_cqes_ext);

    // ── 7. Stash all 9 fields in ShedulerCtx ─────────────────────────────────
    for (val, off) in [
        (fd, ShedulerCtxLayout::IO_URING_FD),
        (sq_tail_ptr, ShedulerCtxLayout::SQ_TAIL_PTR),
        (sq_mask_ptr, ShedulerCtxLayout::SQ_MASK_PTR),
        (sq_array_ptr, ShedulerCtxLayout::SQ_ARRAY_PTR),
        (sqe_array_ptr, ShedulerCtxLayout::SQE_ARRAY_PTR),
        (cq_head_ptr, ShedulerCtxLayout::CQ_HEAD_PTR),
        (cq_tail_ptr, ShedulerCtxLayout::CQ_TAIL_PTR),
        (cq_mask_ptr, ShedulerCtxLayout::CQ_MASK_PTR),
        (cq_cqes_ptr, ShedulerCtxLayout::CQ_CQES_PTR),
    ] {
        let off_v = builder.ins().iconst(ty, off as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_fat, val, ptr_size, off_v]);
    }

    builder.ins().call(free_ref, &[params_fat]);

    // ── 8. Allocate parked-actor list ────────────────────────────────────────
    let parked_bytes = builder
        .ins()
        .iconst(ty, ShedulerCtxLayout::INITIAL_IO_PARKED_CAP * 16);
    let call_parked = builder.ins().call(allocate_ref, &[parked_bytes]);
    let parked_fat = builder.inst_results(call_parked)[0];

    for (val, off) in [
        (parked_fat, ShedulerCtxLayout::IO_PARKED_FAT),
        (zero64, ShedulerCtxLayout::IO_PARKED_COUNT),
    ] {
        let off_v = builder.ins().iconst(ty, off as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_fat, val, ptr_size, off_v]);
    }
    let init_cap = builder
        .ins()
        .iconst(ty, ShedulerCtxLayout::INITIAL_IO_PARKED_CAP);
    let cap_off = builder
        .ins()
        .iconst(ty, ShedulerCtxLayout::IO_PARKED_CAP as i64);
    builder
        .ins()
        .call(store_ref, &[sh_ctx_fat, init_cap, ptr_size, cap_off]);

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_uring_setup", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_io_uring_flush()` — drain any pending SQEs through a single
/// `io_uring_enter`.  Called from the scheduler exit path so the residual
/// from a partial batch isn't lost when the ring fd is closed by the kernel.
pub fn define_io_uring_flush(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_io_uring_flush")),
    };
    let sched_fat_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    let pending = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );

    let has_pending = builder.ins().icmp_imm(IntCC::NotEqual, pending, 0);
    let do_flush = builder.create_block();
    let done = builder.create_block();
    builder.ins().brif(has_pending, do_flush, &[], done, &[]);

    builder.switch_to_block(do_flush);
    builder.seal_block(do_flush);

    let ring_fd = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_URING_FD,
    );
    let nr_enter = builder.ins().iconst(ty, SYS_IO_URING_ENTER);
    let zero64 = builder.ins().iconst(ty, 0);
    let _ = builder.ins().call(
        syscall_ref,
        &[nr_enter, ring_fd, pending, zero64, zero64, zero64, zero64],
    );
    builder.ins().store(
        MemFlags::trusted(),
        zero64,
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );
    builder.ins().jump(done, &[]);

    builder.switch_to_block(done);
    builder.seal_block(done);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_uring_flush", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_write_async(fd, fat_ptr, size)` — fire-and-forget write through
/// io_uring.
///
/// 1. Bounds-check `fat_ptr.start + size <= fat_ptr.end`.
/// 2. Reserve an SQE slot (SQ tail & mask).
/// 3. Fill the SQE: opcode=WRITE, fd, addr, len, user_data=0.
/// 4. Write the SQ array slot (indirect submission queue).
/// 5. Advance SQ tail with a release store.
/// 6. `io_uring_enter(fd, 1, 0, 0, …)` — submit, no wait.
///
/// Park/wake (stages 4-5) is not yet wired: this entry returns to the caller
/// like a regular sync function, but the actual write happens asynchronously
/// in the kernel.  Acceptable for fire-and-forget workloads (logging,
/// /dev/null benchmarks); needs full park before reads are added.
pub fn define_write_async(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_write_async")),
    };

    // Locate the global pointing to the scheduler context fat-ptr struct.
    let sched_fat_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    for _ in 0..3 {
        builder.func.signature.params.push(AbiParam::new(ty));
    }

    let entry = builder.create_block();
    for _ in 0..3 {
        builder.append_block_param(entry, ty);
    }
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let [fd, fat_ptr, size] = builder.block_params(entry)[0..3] else {
        unreachable!()
    };

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);

    // ── Bounds check: fat_ptr.start + size <= fat_ptr.end ────────────────────
    let start = FatPtrLayout::load_start(&mut builder, ty, fat_ptr);
    let end = FatPtrLayout::load_end(&mut builder, ty, fat_ptr);
    let access_end = builder.ins().iadd(start, size);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
    builder
        .ins()
        .trapz(in_bounds, TrapCode::unwrap_user(32));

    // ── Load resolved ring pointers from sh_ctx ──────────────────────────────
    // sh_ctx_fat is a fat-ptr address; deref once to get raw sh_ctx start.
    let sh_ctx_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), sh_ctx_fat, 0);
    let load_field = |b: &mut FunctionBuilder, off: i32| -> cranelift::prelude::Value {
        b.ins().load(ty, MemFlags::trusted(), sh_ctx_start, off)
    };
    let ring_fd = load_field(&mut builder, ShedulerCtxLayout::IO_URING_FD);
    let sq_tail_ptr = load_field(&mut builder, ShedulerCtxLayout::SQ_TAIL_PTR);
    let sq_mask_ptr = load_field(&mut builder, ShedulerCtxLayout::SQ_MASK_PTR);
    let sq_array_ptr = load_field(&mut builder, ShedulerCtxLayout::SQ_ARRAY_PTR);
    let sqe_array_ptr = load_field(&mut builder, ShedulerCtxLayout::SQE_ARRAY_PTR);

    // ── Reserve SQE slot: tail & mask → idx, fill SQE, write SQ array ───────
    let tail = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), sq_tail_ptr, 0);
    let mask = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), sq_mask_ptr, 0);
    let idx32 = builder.ins().band(tail, mask);
    let idx = builder.ins().uextend(ty, idx32);

    let sqe_offset = builder.ins().imul_imm(idx, SQE_BYTES);
    let sqe_addr = builder.ins().iadd(sqe_array_ptr, sqe_offset);

    // Zero the SQE first (64 bytes = 8 × u64).
    let zero64 = builder.ins().iconst(ty, 0);
    for i in 0..8 {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, sqe_addr, i * 8);
    }

    // Fill required fields:
    //   opcode @ 0  (u8), flags @ 1 (u8), ioprio @ 2 (u16) — already 0
    //   fd     @ 4  (i32)
    //   off    @ 8  (u64) — 0 for non-seekable, already zeroed
    //   addr   @ 16 (u64) — buffer pointer
    //   len    @ 24 (u32)
    let opcode = builder.ins().iconst(types::I8, IORING_OP_WRITE);
    builder
        .ins()
        .store(MemFlags::trusted(), opcode, sqe_addr, 0);
    let fd32 = builder.ins().ireduce(types::I32, fd);
    builder
        .ins()
        .store(MemFlags::trusted(), fd32, sqe_addr, 4);
    builder
        .ins()
        .store(MemFlags::trusted(), start, sqe_addr, 16);
    let len32 = builder.ins().ireduce(types::I32, size);
    builder
        .ins()
        .store(MemFlags::trusted(), len32, sqe_addr, 24);

    // SQ.array[idx] = idx — the indirect submission queue.
    let arr_offset = builder.ins().ishl_imm(idx, 2);
    let arr_slot = builder.ins().iadd(sq_array_ptr, arr_offset);
    builder
        .ins()
        .store(MemFlags::trusted(), idx32, arr_slot, 0);

    // Advance SQ.tail with a release store.  On x86-64 the syscall (when one
    // fires below) acts as the full barrier; the userspace-only path leaves
    // a regular store, which is fine because the kernel won't observe the
    // new tail until io_uring_enter is eventually called.
    let new_tail = builder.ins().iadd_imm(tail, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), new_tail, sq_tail_ptr, 0);

    // ── Batch: increment SQE_PENDING; only enter the kernel every Nth call ──
    let pending_off = ShedulerCtxLayout::SQE_PENDING;
    let pending = builder
        .ins()
        .load(ty, MemFlags::trusted(), sh_ctx_start, pending_off);
    let pending_next = builder.ins().iadd_imm(pending, 1);

    let batch_full = builder
        .ins()
        .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, pending_next, SQE_BATCH_SIZE);

    let flush_block = builder.create_block();
    let no_flush_block = builder.create_block();
    let merge_block = builder.create_block();
    builder
        .ins()
        .brif(batch_full, flush_block, &[], no_flush_block, &[]);

    // ── flush_block: io_uring_enter(fd, pending_next, 0, 0); pending = 0 ────
    builder.switch_to_block(flush_block);
    builder.seal_block(flush_block);
    let nr_enter = builder.ins().iconst(ty, SYS_IO_URING_ENTER);
    let _ = builder.ins().call(
        syscall_ref,
        &[nr_enter, ring_fd, pending_next, zero64, zero64, zero64, zero64],
    );
    builder
        .ins()
        .store(MemFlags::trusted(), zero64, sh_ctx_start, pending_off);
    builder.ins().jump(merge_block, &[]);

    // ── no_flush_block: just stash the new pending count ───────────────────
    builder.switch_to_block(no_flush_block);
    builder.seal_block(no_flush_block);
    builder
        .ins()
        .store(MemFlags::trusted(), pending_next, sh_ctx_start, pending_off);
    builder.ins().jump(merge_block, &[]);

    builder.switch_to_block(merge_block);
    builder.seal_block(merge_block);

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_write_async", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
