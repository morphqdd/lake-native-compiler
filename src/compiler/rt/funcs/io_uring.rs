//! io_uring runtime — async I/O backend.
//!
//! Stage 2: real ring setup.  `rt_io_uring_setup(sh_ctx_fat_ptr)` allocates a
//! 120 B `io_uring_params` struct from the heap, calls `io_uring_setup(2)` to
//! create the ring, mmaps the three regions (SQ ring / CQ ring / SQE array)
//! using the offsets the kernel returns, and stashes fd + 3 mmap pointers in
//! the scheduler context.  Also allocates the parked-actor list.
//!
//! All offsets / sizes are documented in `docs/io_uring_design.md`.
//!
//! Stages 3-5 add submit / park / poll on top of this scaffolding.

use anyhow::{Result, anyhow};
use cranelift::{
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags,
        TrapCode,
    },
};

use crate::compiler::{ctx::CompilerCtx, rt::layout::sheduler_ctx::ShedulerCtxLayout};

/// Stop code returned by an actor when it parks on an I/O submission.  The
/// scheduler must NOT call `remove_current_process` for this code — the slot
/// has already been vacated by `io_park_current_actor`.
#[allow(dead_code)]
pub const STOP_PARK: i64 = -3;

// Kernel ABI constants.
const SYS_MMAP: i64 = 9;
const SYS_IO_URING_SETUP: i64 = 425;

const PROT_READ: i64 = 0x1;
const PROT_WRITE: i64 = 0x2;
const MAP_SHARED: i64 = 0x01;
const MAP_POPULATE: i64 = 0x8000;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

// io_uring_params layout (120 bytes total).  Offsets from kernel uapi.
const PARAMS_SIZE: i64 = 120;
const PARAMS_OFF_SQ_ENTRIES: i32 = 0;
const PARAMS_OFF_CQ_ENTRIES: i32 = 4;
// const PARAMS_OFF_FEATURES: i32 = 20;
const PARAMS_OFF_SQ_OFF_ARRAY: i32 = 40 + 24;
const PARAMS_OFF_CQ_OFF_CQES: i32 = 80 + 20;

const SQE_BYTES: i64 = 64;
const CQE_BYTES: i64 = 16;

const RING_ENTRIES: i64 = 256;

/// Build `rt_io_uring_setup(sh_ctx_fat_ptr)` — performs the full
/// setup-and-stash sequence.  Called once from the scheduler entry block
/// after `ShedulerCtxLayout::init` has populated the queue fields.
///
/// Layout of the io_uring_params struct (key offsets only):
///   0   __u32 sq_entries        (kernel writes)
///   4   __u32 cq_entries        (kernel writes)
///   8   __u32 flags             (we write 0)
///  12   __u32 sq_thread_cpu     (we write 0)
///  16   __u32 sq_thread_idle    (we write 0)
///  20   __u32 features          (kernel writes)
///  24   __u32 wq_fd             (we write 0)
///  28   __u32 resv[3]           (we write 0)
///  40   io_sqring_offsets sq_off (kernel writes, 40 B)
///  80   io_cqring_offsets cq_off (kernel writes, 40 B)
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

    // ── 1. Allocate params struct (120 B), get raw payload ptr ───────────────
    let params_size = builder.ins().iconst(ty, PARAMS_SIZE);
    let call_alloc = builder.ins().call(allocate_ref, &[params_size]);
    let params_fat = builder.inst_results(call_alloc)[0];
    let params_start = builder.ins().load(ty, MemFlags::trusted(), params_fat, 0);

    // ── 2. Zero-init params (15 × u64 = 120 B) ───────────────────────────────
    for i in 0..(PARAMS_SIZE / 8) {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, params_start, (i * 8) as i32);
    }

    // ── 3. syscall(SYS_IO_URING_SETUP, RING_ENTRIES, params_start, 0…) ───────
    let nr = builder.ins().iconst(ty, SYS_IO_URING_SETUP);
    let entries = builder.ins().iconst(ty, RING_ENTRIES);
    let call_setup = builder.ins().call(
        syscall_ref,
        &[nr, entries, params_start, zero64, zero64, zero64, zero64],
    );
    let fd = builder.inst_results(call_setup)[0];

    // Trap on negative fd (kernel returned -errno).  No fallback: the language
    // requires kernel ≥ 5.6.
    let fd_ok = builder.ins().icmp_imm(IntCC::SignedGreaterThanOrEqual, fd, 0);
    builder
        .ins()
        .trapz(fd_ok, TrapCode::unwrap_user(40));

    // ── 4. Read offsets from params (raw u32 loads from known positions) ─────
    let sq_entries =
        builder
            .ins()
            .load(cranelift::prelude::types::I32, MemFlags::trusted(), params_start, PARAMS_OFF_SQ_ENTRIES);
    let cq_entries =
        builder
            .ins()
            .load(cranelift::prelude::types::I32, MemFlags::trusted(), params_start, PARAMS_OFF_CQ_ENTRIES);
    let sq_off_array =
        builder
            .ins()
            .load(cranelift::prelude::types::I32, MemFlags::trusted(), params_start, PARAMS_OFF_SQ_OFF_ARRAY);
    let cq_off_cqes =
        builder
            .ins()
            .load(cranelift::prelude::types::I32, MemFlags::trusted(), params_start, PARAMS_OFF_CQ_OFF_CQES);

    let sq_entries_ext = builder.ins().uextend(ty, sq_entries);
    let cq_entries_ext = builder.ins().uextend(ty, cq_entries);
    let sq_off_array_ext = builder.ins().uextend(ty, sq_off_array);
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

    // SQE array length = sq_entries * 64
    let sqe_array_len = builder.ins().imul_imm(sq_entries_ext, SQE_BYTES);
    let off_sqes = builder.ins().iconst(ty, IORING_OFF_SQES);
    let call_sqes = builder.ins().call(
        syscall_ref,
        &[nr_mmap, zero64, sqe_array_len, prot, flags_mmap, fd, off_sqes],
    );
    let sqe_array_ptr = builder.inst_results(call_sqes)[0];

    // Trap on mmap failure (a negative return with magnitude small enough to
    // be an errno; we use `addr < 4096` as a simple sentinel for MAP_FAILED-ish).
    for ptr in [sq_ring_ptr, cq_ring_ptr, sqe_array_ptr] {
        let ok = builder
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, ptr, 4096);
        builder.ins().trapz(ok, TrapCode::unwrap_user(41));
    }

    // ── 6. Stash fd + 3 mmap pointers in ShedulerCtx ─────────────────────────
    for (val, off) in [
        (fd, ShedulerCtxLayout::IO_URING_FD),
        (sq_ring_ptr, ShedulerCtxLayout::SQ_RING_PTR),
        (cq_ring_ptr, ShedulerCtxLayout::CQ_RING_PTR),
        (sqe_array_ptr, ShedulerCtxLayout::SQE_ARRAY_PTR),
    ] {
        let off_v = builder.ins().iconst(ty, off as i64);
        builder
            .ins()
            .call(store_ref, &[sh_ctx_fat, val, ptr_size, off_v]);
    }

    // ── 7. Free the params struct (kernel has copied what it needs) ──────────
    builder.ins().call(free_ref, &[params_fat]);

    // ── 8. Allocate parked-actor list (64 × 16 B = 1 KiB) and init counters ─
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
