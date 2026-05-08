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
    codegen::ir::{BlockArg, StackSlot, StackSlotData, StackSlotKind},
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags,
        TrapCode, Value, types,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{
        FatPtrLayout, exec_ctx::ExecCtxLayout, process_ctx::ProcessCtxLayout,
        sheduler_ctx::ShedulerCtxLayout,
    },
};

// STOP_PARK lives in `pipeline::machine` (= -4) — see comment on STOP_PARK
// constant there.  Scheduler treats it as "actor parked, slot vacated, just
// continue the loop without remove or advance".

// Kernel ABI constants.
const SYS_MMAP: i64 = 9;
const SYS_CLOSE: i64 = 3;
const SYS_SOCKET: i64 = 41;
const SYS_BIND: i64 = 49;
const SYS_LISTEN: i64 = 50;
const SYS_SETSOCKOPT: i64 = 54;
const SYS_IO_URING_SETUP: i64 = 425;
const SYS_IO_URING_ENTER: i64 = 426;

const AF_INET: i64 = 2;
const SOCK_STREAM: i64 = 1;
const SOL_SOCKET: i64 = 1;
const SO_REUSEADDR: i64 = 2;

const PROT_READ: i64 = 0x1;
const PROT_WRITE: i64 = 0x2;
const MAP_SHARED: i64 = 0x01;
const MAP_POPULATE: i64 = 0x8000;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

const IORING_OP_WRITE: i64 = 23;
const IORING_OP_SEND: i64 = 26;
const IORING_OP_ACCEPT: i64 = 13;

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

    // user_data @ 32 (u64) = current proc-ctx fat-ptr.  Echoed verbatim in
    // the CQE, used by `emit_wake_by_user_data` to wake the right actor.
    let cur_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let proc_arr_fat_for_ud = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start_for_ud =
        builder.ins().load(ty, MemFlags::trusted(), proc_arr_fat_for_ud, 0);
    let cur_off = builder.ins().ishl_imm(cur_idx, 3);
    let cur_addr = builder.ins().iadd(proc_arr_start_for_ud, cur_off);
    let cur_proc_ctx = builder.ins().load(ty, MemFlags::trusted(), cur_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), cur_proc_ctx, sqe_addr, 32);

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

/// Emit IR that wakes the actor identified by `user_data` if it is currently
/// in `io_parked`.  Linear scan up to `IO_PARKED_COUNT`; on match, swap-and-pop
/// the slot, append the proc-ctx fat-ptr back into `process_arr` (using the
/// dynamic-grow helper), and increment REAL_COUNT.  Silent no-op when
/// `user_data` is 0 (sentinel for fire-and-forget submissions) or when no
/// match is found.
///
/// Inlined into `rt_io_uring_poll_cq`; not exposed as a runtime function.
fn emit_wake_by_user_data(
    sh_ctx_start: Value,
    user_data: Value,
    res: Value,
    ty: cranelift::prelude::Type,
    builder: &mut FunctionBuilder,
) {
    // Bail when user_data == 0 (no actor was parked for this CQE).
    let nonzero = builder.ins().icmp_imm(IntCC::NotEqual, user_data, 0);
    let scan_block = builder.create_block();
    let scan_done = builder.create_block();
    builder
        .ins()
        .brif(nonzero, scan_block, &[], scan_done, &[]);

    // ── scan_block: walk io_parked[0..count] ────────────────────────────────
    builder.switch_to_block(scan_block);
    builder.seal_block(scan_block);
    let parked_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_FAT,
    );
    let parked_start = builder.ins().load(ty, MemFlags::trusted(), parked_fat, 0);
    let parked_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_COUNT,
    );

    // Loop: i = 0; while i < count: cmp slot==user_data
    let loop_hdr = builder.create_block();
    let loop_body = builder.create_block();
    let found = builder.create_block();
    builder.append_block_param(loop_hdr, ty);

    let zero = builder.ins().iconst(ty, 0);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(zero)]);

    builder.switch_to_block(loop_hdr);
    let i = builder.block_params(loop_hdr)[0];
    let cont = builder.ins().icmp(IntCC::UnsignedLessThan, i, parked_count);
    builder.ins().brif(cont, loop_body, &[], scan_done, &[]);

    builder.switch_to_block(loop_body);
    builder.seal_block(loop_body);
    let slot_off = builder.ins().ishl_imm(i, 3);
    let slot_addr = builder.ins().iadd(parked_start, slot_off);
    let slot_val = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 0);
    let eq = builder.ins().icmp(IntCC::Equal, slot_val, user_data);
    let i_next = builder.ins().iadd_imm(i, 1);
    builder.ins().brif(
        eq,
        found,
        &[BlockArg::Value(i)],
        loop_hdr,
        &[BlockArg::Value(i_next)],
    );
    builder.append_block_param(found, ty);
    builder.seal_block(loop_hdr);

    // ── found: swap-and-pop io_parked, append to process_arr, REAL_COUNT++ ──
    builder.switch_to_block(found);
    builder.seal_block(found);
    let match_idx = builder.block_params(found)[0];
    let last_idx = builder.ins().iadd_imm(parked_count, -1);
    let last_off = builder.ins().ishl_imm(last_idx, 3);
    let last_addr = builder.ins().iadd(parked_start, last_off);
    let last_val = builder.ins().load(ty, MemFlags::trusted(), last_addr, 0);
    let match_off = builder.ins().ishl_imm(match_idx, 3);
    let match_addr = builder.ins().iadd(parked_start, match_off);
    builder
        .ins()
        .store(MemFlags::trusted(), last_val, match_addr, 0);
    builder.ins().store(
        MemFlags::trusted(),
        last_idx,
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_COUNT,
    );

    // Deliver CQE.res into ExecCtx.TEMP_VAL so the resumed CPS block can
    // read it as the value of the parked async op (e.g.
    // `let conn = accept(srv)` reads accept's result from TEMP_VAL).
    //   slot_val (= proc-ctx fat-ptr) → start = process_ctx data
    //   process_ctx[EXEC_CTX] = exec-ctx fat-ptr
    //   *exec-ctx fat-ptr = exec_ctx data
    //   exec_ctx[TEMP_VAL] = res (sign-extended from i32 → ptr_ty)
    let proc_ctx_data = builder.ins().load(ty, MemFlags::trusted(), slot_val, 0);
    let exec_ctx_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        proc_ctx_data,
        ProcessCtxLayout::EXEC_CTX,
    );
    let exec_ctx_data = builder.ins().load(ty, MemFlags::trusted(), exec_ctx_fat, 0);
    let res_ext = builder.ins().sextend(ty, res);
    builder
        .ins()
        .store(MemFlags::trusted(), res_ext, exec_ctx_data, ExecCtxLayout::TEMP_VAL);

    // Append slot_val (the woken proc-ctx fat-ptr) back to process_arr.
    let proc_arr_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start = builder.ins().load(ty, MemFlags::trusted(), proc_arr_fat, 0);
    let last_proc = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::LAST_PROCESS_INDEX,
    );
    let next_proc = builder.ins().iadd_imm(last_proc, 1);
    let next_off = builder.ins().ishl_imm(next_proc, 3);
    let dst = builder.ins().iadd(proc_arr_start, next_off);
    // NB: skip grow-check here — caller (poll_cq) is invoked at scheduler
    //     boundary where we trust capacity.  TODO: if io_parked grows past
    //     process_arr cap, hit the grow path properly.
    builder.ins().store(MemFlags::trusted(), slot_val, dst, 0);
    builder.ins().store(
        MemFlags::trusted(),
        next_proc,
        sh_ctx_start,
        ShedulerCtxLayout::LAST_PROCESS_INDEX,
    );
    let real_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );
    let real_count_inc = builder.ins().iadd_imm(real_count, 1);
    builder.ins().store(
        MemFlags::trusted(),
        real_count_inc,
        sh_ctx_start,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );
    builder.ins().jump(scan_done, &[]);

    builder.switch_to_block(scan_done);
    builder.seal_block(scan_done);
}

/// Build `rt_io_uring_wait_cqe()` — combined submit + wait through a single
/// `io_uring_enter` syscall.  Submits any SQEs queued via `emit_submit_sqe`
/// (count tracked in SQE_PENDING) AND blocks until at least one CQE arrives.
/// Resets SQE_PENDING to 0 on return.  Used by the scheduler when there's
/// nothing runnable but actors parked on I/O.
///
/// Halves the syscall rate vs separate submit + wait: previously each
/// accept/send pair did one syscall to submit and another to wait (two per
/// connection); now both fold into one.
pub fn define_io_uring_wait_cqe(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_io_uring_wait_cqe")),
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
    let sh_ctx_start = builder.ins().load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    let ring_fd = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_URING_FD,
    );
    let pending = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );

    // io_uring_enter(fd, to_submit=pending, min_complete=1,
    //                flags=IORING_ENTER_GETEVENTS)
    let nr = builder.ins().iconst(ty, SYS_IO_URING_ENTER);
    let zero = builder.ins().iconst(ty, 0);
    let one = builder.ins().iconst(ty, 1);
    let flags = builder.ins().iconst(ty, 1); // IORING_ENTER_GETEVENTS = 1
    let _ = builder.ins().call(
        syscall_ref,
        &[nr, ring_fd, pending, one, flags, zero, zero],
    );
    builder.ins().store(
        MemFlags::trusted(),
        zero,
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_uring_wait_cqe", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_io_park_current()` — moves the currently-running actor from
/// `process_arr` into `io_parked` so the scheduler skips it on the next
/// round-robin tick.
///
/// Caller responsibilities (handled by the frontend's park-aware codegen):
///   1. Submit an SQE whose `user_data` matches the current proc-ctx fat-ptr
///      so the wake path can find it on completion.
///   2. Store the resume `BLOCK_ID` into ExecCtx **before** calling this fn.
///   3. After this returns, jump to `quantum_continue` with `STOP_PARK` so
///      the machine returns -4 to the scheduler.
pub fn define_io_park_current(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

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

    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder.ins().load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    // Read CURRENT_PROCESS, LAST_PROCESS_INDEX, REAL_COUNT, PROCESS_ARR_FAT.
    let current_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let last_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::LAST_PROCESS_INDEX,
    );
    let real_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );
    let proc_arr_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start = builder.ins().load(ty, MemFlags::trusted(), proc_arr_fat, 0);

    // proc_ctx fat-ptr to park = process_arr[current * 8]
    let cur_off = builder.ins().ishl_imm(current_idx, 3);
    let cur_addr = builder.ins().iadd(proc_arr_start, cur_off);
    let proc_ctx = builder.ins().load(ty, MemFlags::trusted(), cur_addr, 0);

    // swap-and-pop: process_arr[current] = process_arr[last]; LAST -= 1
    let last_off = builder.ins().ishl_imm(last_idx, 3);
    let last_addr = builder.ins().iadd(proc_arr_start, last_off);
    let last_val = builder.ins().load(ty, MemFlags::trusted(), last_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), last_val, cur_addr, 0);
    let new_last = builder.ins().iadd_imm(last_idx, -1);
    builder.ins().store(
        MemFlags::trusted(),
        new_last,
        sh_ctx_start,
        ShedulerCtxLayout::LAST_PROCESS_INDEX,
    );

    // Reset CURRENT_PROCESS to 0 — same convention as remove_current_process.
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().store(
        MemFlags::trusted(),
        zero,
        sh_ctx_start,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );

    let new_real = builder.ins().iadd_imm(real_count, -1);
    builder.ins().store(
        MemFlags::trusted(),
        new_real,
        sh_ctx_start,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );

    // Append proc_ctx to io_parked.  No grow here — we trust the cap is
    // sufficient since process_arr can never hold more entries than its
    // own cap, which is at least as large as IO_PARKED_CAP from setup.
    // TODO: replace with a proper grow path when bench scales beyond 64
    // simultaneously-parked actors.
    let parked_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_FAT,
    );
    let parked_start = builder.ins().load(ty, MemFlags::trusted(), parked_fat, 0);
    let parked_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_COUNT,
    );
    let parked_off = builder.ins().ishl_imm(parked_count, 3);
    let parked_dst = builder.ins().iadd(parked_start, parked_off);
    builder
        .ins()
        .store(MemFlags::trusted(), proc_ctx, parked_dst, 0);
    let new_parked = builder.ins().iadd_imm(parked_count, 1);
    builder.ins().store(
        MemFlags::trusted(),
        new_parked,
        sh_ctx_start,
        ShedulerCtxLayout::IO_PARKED_COUNT,
    );

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_park_current", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_io_uring_poll_cq()` — drains all pending CQEs.  For each, reads
/// `user_data` and wakes the corresponding parked actor.  Called from the
/// scheduler loop on every iteration.
pub fn define_io_uring_poll_cq(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

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

    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    let cq_head_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CQ_HEAD_PTR,
    );
    let cq_tail_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CQ_TAIL_PTR,
    );
    let cq_mask_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CQ_MASK_PTR,
    );
    let cq_cqes_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CQ_CQES_PTR,
    );

    // Read CQ.tail with acquire semantics — kernel produces, we consume.
    let mask = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), cq_mask_ptr, 0);
    let mask_ext = builder.ins().uextend(ty, mask);

    // Loop: head iterates with block param.
    let loop_hdr = builder.create_block();
    let loop_body = builder.create_block();
    let loop_exit = builder.create_block();
    builder.append_block_param(loop_hdr, types::I32);

    let head_init = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), cq_head_ptr, 0);
    builder
        .ins()
        .jump(loop_hdr, &[BlockArg::Value(head_init)]);

    builder.switch_to_block(loop_hdr);
    let head = builder.block_params(loop_hdr)[0];
    let tail = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), cq_tail_ptr, 0);
    let cont = builder.ins().icmp(IntCC::NotEqual, head, tail);
    builder.ins().brif(cont, loop_body, &[], loop_exit, &[]);

    builder.switch_to_block(loop_body);
    builder.seal_block(loop_body);
    // cqe_addr = CQ_CQES + (head & mask) * 16
    let head_ext = builder.ins().uextend(ty, head);
    let idx = builder.ins().band(head_ext, mask_ext);
    let cqe_off = builder.ins().imul_imm(idx, CQE_BYTES);
    let cqe_addr = builder.ins().iadd(cq_cqes_ptr, cqe_off);
    // user_data is the first 8 bytes of the CQE; res is the next i32.
    let user_data = builder.ins().load(ty, MemFlags::trusted(), cqe_addr, 0);
    let res = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), cqe_addr, 8);

    emit_wake_by_user_data(sh_ctx_start, user_data, res, ty, &mut builder);

    let head_next = builder.ins().iadd_imm(head, 1);
    builder
        .ins()
        .jump(loop_hdr, &[BlockArg::Value(head_next)]);
    builder.seal_block(loop_hdr);

    builder.switch_to_block(loop_exit);
    builder.seal_block(loop_exit);
    // Store the consumed head back with release semantics (regular store).
    builder
        .ins()
        .store(MemFlags::trusted(), head, cq_head_ptr, 0);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_uring_poll_cq", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_listen_tcp(port: i64) -> fd: i64`.
///
/// Combined helper: creates an IPv4 TCP socket, sets `SO_REUSEADDR`, binds to
/// `0.0.0.0:port`, and listens with backlog 128.  Sync syscalls only (no
/// io_uring) — server bring-up is one-shot.
pub fn define_listen_tcp(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_listen_tcp")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    // sockaddr_in is 16 B; optval (int=1 for SO_REUSEADDR) is 4 B.  Allocate
    // a single 32 B explicit stack slot — first 16 B = sockaddr, last 16 B
    // for optval (only the leading 4 B used).
    let scratch: StackSlot = builder
        .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 4));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let port = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let sa_addr = builder.ins().stack_addr(ty, scratch, 0);
    let opt_addr = builder.ins().stack_addr(ty, scratch, 16);

    let zero = builder.ins().iconst(ty, 0);

    // Zero the 32 B scratch.
    for off in (0..32).step_by(8) {
        builder
            .ins()
            .store(MemFlags::trusted(), zero, sa_addr, off);
    }

    // sockaddr_in:
    //   0   sin_family = AF_INET (u16)
    //   2   sin_port   = htons(port) (u16, big-endian)
    //   4   sin_addr   = 0 (INADDR_ANY)
    //   8.. zero
    let af_inet = builder.ins().iconst(types::I16, AF_INET);
    builder
        .ins()
        .store(MemFlags::trusted(), af_inet, sa_addr, 0);

    // htons(port): swap low/high bytes of u16.
    let port16 = builder.ins().ireduce(types::I16, port);
    let port_be = builder.ins().bswap(port16);
    builder
        .ins()
        .store(MemFlags::trusted(), port_be, sa_addr, 2);

    // optval = (int)1
    let one32 = builder.ins().iconst(types::I32, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), one32, opt_addr, 0);

    // socket(AF_INET, SOCK_STREAM, 0) → fd
    let nr_socket = builder.ins().iconst(ty, SYS_SOCKET);
    let af = builder.ins().iconst(ty, AF_INET);
    let sock_stream = builder.ins().iconst(ty, SOCK_STREAM);
    let call_sock = builder.ins().call(
        syscall_ref,
        &[nr_socket, af, sock_stream, zero, zero, zero, zero],
    );
    let fd = builder.inst_results(call_sock)[0];
    let fd_ok = builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, fd, 0);
    builder.ins().trapz(fd_ok, TrapCode::unwrap_user(50));

    // setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &1, 4)
    let nr_setsockopt = builder.ins().iconst(ty, SYS_SETSOCKOPT);
    let sol = builder.ins().iconst(ty, SOL_SOCKET);
    let reuse = builder.ins().iconst(ty, SO_REUSEADDR);
    let four = builder.ins().iconst(ty, 4);
    let _ = builder.ins().call(
        syscall_ref,
        &[nr_setsockopt, fd, sol, reuse, opt_addr, four, zero],
    );

    // bind(fd, sa, 16)
    let nr_bind = builder.ins().iconst(ty, SYS_BIND);
    let sixteen = builder.ins().iconst(ty, 16);
    let call_bind = builder.ins().call(
        syscall_ref,
        &[nr_bind, fd, sa_addr, sixteen, zero, zero, zero],
    );
    let bind_rc = builder.inst_results(call_bind)[0];
    let bind_ok = builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, bind_rc, 0);
    builder.ins().trapz(bind_ok, TrapCode::unwrap_user(51));

    // listen(fd, 128)
    let nr_listen = builder.ins().iconst(ty, SYS_LISTEN);
    let backlog = builder.ins().iconst(ty, 128);
    let _ = builder.ins().call(
        syscall_ref,
        &[nr_listen, fd, backlog, zero, zero, zero, zero],
    );

    builder.ins().return_(&[fd]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_listen_tcp", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_close(fd: i64)`.
pub fn define_close(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_close")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let fd = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    let nr = builder.ins().iconst(ty, SYS_CLOSE);
    let zero = builder.ins().iconst(ty, 0);
    let _ = builder
        .ins()
        .call(syscall_ref, &[nr, fd, zero, zero, zero, zero, zero]);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_close", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Emit a generic SQE-fill at the next SQ tail slot for a 4-arg op shape:
/// {opcode, fd, addr, len}.  `addr_ext` may be 0 for ops that don't use it
/// (e.g. accept).  Returns `()` — caller is responsible for the io_uring_enter
/// wakeup if any.
fn emit_submit_sqe(
    sh_ctx_start: Value,
    opcode_imm: i64,
    fd: Value,
    addr_ext: Value,
    len_ext: Value,
    ty: cranelift::prelude::Type,
    builder: &mut FunctionBuilder,
) {
    let sq_tail_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQ_TAIL_PTR,
    );
    let sq_mask_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQ_MASK_PTR,
    );
    let sq_array_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQ_ARRAY_PTR,
    );
    let sqe_array_ptr = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQE_ARRAY_PTR,
    );

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

    let zero64 = builder.ins().iconst(ty, 0);
    for i in 0..8 {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, sqe_addr, i * 8);
    }

    let opcode = builder.ins().iconst(types::I8, opcode_imm);
    builder
        .ins()
        .store(MemFlags::trusted(), opcode, sqe_addr, 0);
    let fd32 = builder.ins().ireduce(types::I32, fd);
    builder
        .ins()
        .store(MemFlags::trusted(), fd32, sqe_addr, 4);
    builder
        .ins()
        .store(MemFlags::trusted(), addr_ext, sqe_addr, 16);
    let len32 = builder.ins().ireduce(types::I32, len_ext);
    builder
        .ins()
        .store(MemFlags::trusted(), len32, sqe_addr, 24);

    // user_data = current proc-ctx fat-ptr
    let cur_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let proc_arr_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start = builder.ins().load(ty, MemFlags::trusted(), proc_arr_fat, 0);
    let cur_off = builder.ins().ishl_imm(cur_idx, 3);
    let cur_addr = builder.ins().iadd(proc_arr_start, cur_off);
    let proc_ctx = builder.ins().load(ty, MemFlags::trusted(), cur_addr, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), proc_ctx, sqe_addr, 32);

    // SQ.array[idx] = idx
    let arr_off = builder.ins().ishl_imm(idx, 2);
    let arr_slot = builder.ins().iadd(sq_array_ptr, arr_off);
    builder
        .ins()
        .store(MemFlags::trusted(), idx32, arr_slot, 0);

    let new_tail = builder.ins().iadd_imm(tail, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), new_tail, sq_tail_ptr, 0);

    // Bump SQE_PENDING — every caller goes through here, so this is the
    // single source of truth for "how many SQEs are queued but not yet
    // submitted to the kernel".  The scheduler folds the submit count into
    // its combined `io_uring_enter(fd, pending, 1, GETEVENTS)` wait, so
    // submit-and-park paths never issue their own syscall.
    let pending = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );
    let pending_next = builder.ins().iadd_imm(pending, 1);
    builder.ins().store(
        MemFlags::trusted(),
        pending_next,
        sh_ctx_start,
        ShedulerCtxLayout::SQE_PENDING,
    );
}

/// Build `rt_accept_async(fd: i64)` — a parking accept.
///
/// Emits a `IORING_OP_ACCEPT` SQE with `addr=0`, `len=0` (don't care about
/// peer address for now), submits via `io_uring_enter(min_complete=0)` to
/// nudge the kernel, then returns to the caller.  The frontend's park-aware
/// codegen runs the park epilogue around this call (set BLOCK_ID, jump
/// STOP_PARK), and on CQE the woken actor reads its new conn fd from
/// ExecCtx.TEMP_VAL.
pub fn define_accept_async(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_accept_async")),
    };
    let sched_fat_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found")),
    };
    let park_id = match ctx.module().get_name("rt_io_park_current") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_io_park_current must be declared before rt_accept_async")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let fd = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let park_ref = ctx
        .module_mut()
        .declare_func_in_func(park_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder.ins().load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    let zero = builder.ins().iconst(ty, 0);

    emit_submit_sqe(sh_ctx_start, IORING_OP_ACCEPT, fd, zero, zero, ty, &mut builder);

    // No explicit io_uring_enter — emit_submit_sqe bumped SQE_PENDING; the
    // scheduler's combined wait+submit on its next park-tick will fold this
    // SQE into a single syscall along with any other queued submissions.
    let _ = syscall_ref;

    // Park the current actor — caller's frontend epilogue will set BLOCK_ID
    // and return STOP_PARK from the machine.
    builder.ins().call(park_ref, &[]);

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_accept_async", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_send(fd, fat_ptr, size)` — parking send.  Submits an
/// IORING_OP_SEND SQE, immediately enters the kernel to nudge submission,
/// then parks the current actor.  On CQE the actor wakes with the bytes-sent
/// count in ExecCtx.TEMP_VAL.
///
/// We park on every send (rather than batching) so user code can safely do
/// `rt_send(fd, …); rt_close(fd)` without racing the close against an
/// un-flushed SQE.  Higher-throughput batched send can be added later as a
/// separate `rt_send_async` if needed.
pub fn define_send_async(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_send_async")),
    };
    let sched_fat_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found")),
    };
    let park_id = match ctx.module().get_name("rt_io_park_current") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_io_park_current must be declared before rt_send_async")),
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
    let park_ref = ctx
        .module_mut()
        .declare_func_in_func(park_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder.ins().load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    // Bounds check + extract payload start.
    let start = FatPtrLayout::load_start(&mut builder, ty, fat_ptr);
    let end = FatPtrLayout::load_end(&mut builder, ty, fat_ptr);
    let access_end = builder.ins().iadd(start, size);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
    builder
        .ins()
        .trapz(in_bounds, TrapCode::unwrap_user(32));

    emit_submit_sqe(sh_ctx_start, IORING_OP_SEND, fd, start, size, ty, &mut builder);

    // No explicit io_uring_enter (see rt_accept_async).  Pending count is
    // bumped by emit_submit_sqe; the scheduler's combined wait+submit
    // syscall handles the actual submission.
    let _ = syscall_ref;
    builder.ins().call(park_ref, &[]);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_send_async", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}


