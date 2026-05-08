//! io_uring runtime — async I/O backend.
//!
//! Stage 1: stub `rt_io_uring_setup` (no-op) so the rest of the runtime can
//! be wired up without crashing.  Real ring creation lands in stage 2.
//!
//! Full design: `docs/io_uring_design.md`.

use anyhow::Result;
use cranelift::{
    module::{Linkage, Module},
    prelude::{FunctionBuilder, FunctionBuilderContext, InstBuilder},
};

use crate::compiler::ctx::CompilerCtx;

/// Stop code returned by an actor when it parks on an I/O submission.  The
/// scheduler must NOT call `remove_current_process` for this code — the slot
/// has already been vacated by `io_park_current_actor`.  Just continue.
///
/// Scheduler dispatch sees this value as a return from the exec block and
/// distinguishes it from `STOP_DONE = -1` and `STOP_LIMIT = -2`.
#[allow(dead_code)]
pub const STOP_PARK: i64 = -3;

/// Build `rt_io_uring_setup()` — stage 1 stub.
///
/// Real implementation will:
///   1. Allocate `io_uring_params` struct on the heap (zero-init).
///   2. Call `syscall(SYS_io_uring_setup=425, 256, &params)` → ring fd.
///   3. mmap three regions (SQ ring / CQ ring / SQE array).
///   4. Store fd + 3 mmap pointers in ShedulerCtx fields IO_URING_FD,
///      SQ_RING_PTR, CQ_RING_PTR, SQE_ARRAY_PTR.
///   5. Initialise IO_PARKED_FAT (heap-alloc 64 × 16 B = 1 KiB) and
///      IO_PARKED_COUNT/CAP fields.
///
/// Stage 1 just declares the function so callers compile and link.
pub fn define_io_uring_setup(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_io_uring_setup", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
