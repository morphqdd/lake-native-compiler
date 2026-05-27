//! Async child-process primitives — pidfd + io_uring POLLADD.
//!
//! Phase 1 of #099.  Sync `SYS_WAIT4` froze the whole scheduler for the
//! child's lifetime; see `docs/state/features/099_async_pidfd.md`.

use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::{BlockArg, StackSlotData, StackSlotKind},
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, Value,
        types,
    },
};

use crate::compiler::{
    ctx::CompilerCtx, rt::layout::sheduler_ctx::ShedulerCtxLayout, target::LinuxSyscalls,
};

// clone(2) flags + clone_args field offsets.
const CLONE_PIDFD: i64 = 0x00001000;
const SIGCHLD: i64 = 17;
const CLONE_ARGS_SIZE: i64 = 88;
const CLONE_ARGS_FLAGS: i32 = 0;
const CLONE_ARGS_PIDFD: i32 = 8;
const CLONE_ARGS_EXIT_SIGNAL: i32 = 32;

// io_uring SQE opcode + POLL field.
const IORING_OP_POLL_ADD: i64 = 6;
const POLLIN: i64 = 0x1;
const SQE_BYTES: i64 = 64;

// waitid id-types and option flags.
const P_PIDFD: i64 = 3;
const WEXITED: i64 = 0x4;
const WNOHANG: i64 = 0x1;

/// `rt_clone3_pidfd(_unused: i64) -> i64`
///
/// Issues clone3 with `CLONE_PIDFD | SIGCHLD`; the kernel writes the pidfd
/// into an on-stack `clone_args.pidfd` slot.  Returns the pidfd to the
/// parent, 0 to the child, or a negative errno on failure.
///
/// Stack & FDs are shared COW-style with fork — child can exec or _exit
/// immediately without bookkeeping.  Unused arg reserved for future flags.
pub fn define_clone3_pidfd(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_syscall must be declared before rt_clone3_pidfd"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let _unused = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    // Stack-allocate clone_args struct (88 bytes, 8-byte aligned).
    let args_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        CLONE_ARGS_SIZE as u32,
        8,
    ));
    let args_addr = builder.ins().stack_addr(ty, args_slot, 0);
    let zero64 = builder.ins().iconst(ty, 0);
    for i in 0..(CLONE_ARGS_SIZE / 8) {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, args_addr, (i * 8) as i32);
    }

    // Separate u64-sized pidfd cell — kernel writes a 32-bit int here.
    let pidfd_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 4));
    let pidfd_addr = builder.ins().stack_addr(ty, pidfd_slot, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), zero64, pidfd_addr, 0);

    let flags = builder.ins().iconst(ty, CLONE_PIDFD);
    builder
        .ins()
        .store(MemFlags::trusted(), flags, args_addr, CLONE_ARGS_FLAGS);
    builder
        .ins()
        .store(MemFlags::trusted(), pidfd_addr, args_addr, CLONE_ARGS_PIDFD);
    let exit_sig = builder.ins().iconst(ty, SIGCHLD);
    builder.ins().store(
        MemFlags::trusted(),
        exit_sig,
        args_addr,
        CLONE_ARGS_EXIT_SIGNAL,
    );

    let nr = builder
        .ins()
        .iconst(ty, ctx.syscalls().sys_clone3);
    let size = builder.ins().iconst(ty, CLONE_ARGS_SIZE);
    let call = builder.ins().call(
        syscall_ref,
        &[nr, args_addr, size, zero64, zero64, zero64, zero64],
    );
    let rv = builder.inst_results(call)[0];

    let err_block = builder.create_block();
    let ok_block = builder.create_block();
    let parent_block = builder.create_block();
    let ret_block = builder.create_block();
    builder.append_block_param(ret_block, ty);

    let is_neg = builder.ins().icmp_imm(IntCC::SignedLessThan, rv, 0);
    builder
        .ins()
        .brif(is_neg, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    builder.ins().jump(ret_block, &[BlockArg::Value(rv)]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    let is_child = builder.ins().icmp_imm(IntCC::Equal, rv, 0);
    builder.ins().brif(
        is_child,
        ret_block,
        &[BlockArg::Value(zero64)],
        parent_block,
        &[],
    );

    builder.switch_to_block(parent_block);
    builder.seal_block(parent_block);
    let pidfd32 = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), pidfd_addr, 0);
    let pidfd64 = builder.ins().uextend(ty, pidfd32);
    builder.ins().jump(ret_block, &[BlockArg::Value(pidfd64)]);

    builder.switch_to_block(ret_block);
    builder.seal_block(ret_block);
    let result = builder.block_params(ret_block)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_clone3_pidfd", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// `rt_pidfd_poll_async(pidfd: i64) -> i64`
///
/// Submits `IORING_OP_POLL_ADD` for `POLLIN` on `pidfd`, stamps the
/// current proc-ctx fat-ptr into `user_data` (parking variant — wake
/// machinery routes CQE.res into TEMP_VAL), then parks the actor.
/// CQE fires when the child exits.
pub fn define_pidfd_poll_async(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let sched_fat_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global not found")),
    };
    let park_id = match ctx.module().get_name("rt_io_park_current") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_io_park_current must be declared before rt_pidfd_poll_async"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let pidfd = builder.block_params(entry)[0];

    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_fat_id, &mut builder.func);
    let park_ref = ctx
        .module_mut()
        .declare_func_in_func(park_id, &mut builder.func);
    let sh_ctx_fat = builder.ins().global_value(ty, sched_gv);
    let sh_ctx_start = builder.ins().load(ty, MemFlags::trusted(), sh_ctx_fat, 0);

    emit_submit_poll_sqe(sh_ctx_start, pidfd, ty, &mut builder);

    builder.ins().call(park_ref, &[]);

    // Wake path stuffs CQE.res into TEMP_VAL — return value here is a
    // placeholder cranelift requires for the signature.
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().return_(&[zero]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_pidfd_poll_async", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// POLLADD-shaped SQE — like io_uring.rs:emit_submit_sqe but writes
/// `poll32_events` @ 28 instead of `len` @ 24.  Stamps proc-ctx fat-ptr
/// into user_data so emit_wake_by_user_data routes the CQE back.
fn emit_submit_poll_sqe(
    sh_ctx_start: Value,
    fd: Value,
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

    let opcode = builder.ins().iconst(types::I8, IORING_OP_POLL_ADD);
    builder
        .ins()
        .store(MemFlags::trusted(), opcode, sqe_addr, 0);
    let fd32 = builder.ins().ireduce(types::I32, fd);
    builder.ins().store(MemFlags::trusted(), fd32, sqe_addr, 4);
    let pollin32 = builder.ins().iconst(types::I32, POLLIN);
    builder
        .ins()
        .store(MemFlags::trusted(), pollin32, sqe_addr, 28);

    // user_data = current proc-ctx fat-ptr (parking variant; see #116).
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

    let arr_off = builder.ins().ishl_imm(idx, 2);
    let arr_slot = builder.ins().iadd(sq_array_ptr, arr_off);
    builder.ins().store(MemFlags::trusted(), idx32, arr_slot, 0);
    let new_tail = builder.ins().iadd_imm(tail, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), new_tail, sq_tail_ptr, 0);
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

/// `rt_waitid_pidfd(pidfd: i64) -> i64`
///
/// Synchronous `waitid(P_PIDFD, pidfd, &siginfo, WEXITED|WNOHANG, NULL)`.
/// Called only after rt_pidfd_poll_async woke on POLLIN, so the pidfd is
/// guaranteed ready and WNOHANG never spins.  Returns child's si_status
/// (0..=255 for clean exit) on success, or negative errno on failure.
pub fn define_waitid_pidfd(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_waitid_pidfd")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);
    let pidfd = builder.block_params(entry)[0];

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    // siginfo_t is at most 128 bytes — stack-alloc and zero.
    let info_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        128,
        8,
    ));
    let info_addr = builder.ins().stack_addr(ty, info_slot, 0);
    let zero64 = builder.ins().iconst(ty, 0);
    for i in 0..16 {
        builder
            .ins()
            .store(MemFlags::trusted(), zero64, info_addr, i * 8);
    }

    let nr = builder
        .ins()
        .iconst(ty, ctx.syscalls().sys_waitid);
    let idtype = builder.ins().iconst(ty, P_PIDFD);
    let options = builder.ins().iconst(ty, WEXITED | WNOHANG);
    let call = builder.ins().call(
        syscall_ref,
        &[nr, idtype, pidfd, info_addr, options, zero64, zero64],
    );
    let rv = builder.inst_results(call)[0];

    let err_block = builder.create_block();
    let ok_block = builder.create_block();
    let ret_block = builder.create_block();
    builder.append_block_param(ret_block, ty);

    let is_neg = builder.ins().icmp_imm(IntCC::SignedLessThan, rv, 0);
    builder
        .ins()
        .brif(is_neg, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    builder.ins().jump(ret_block, &[BlockArg::Value(rv)]);

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    // si_status is i32 at offset 24 in SIGCHLD siginfo_t (Linux x86_64).
    let status32 = builder
        .ins()
        .load(types::I32, MemFlags::trusted(), info_addr, 24);
    let status64 = builder.ins().sextend(ty, status32);
    builder.ins().jump(ret_block, &[BlockArg::Value(status64)]);

    builder.switch_to_block(ret_block);
    builder.seal_block(ret_block);
    let result = builder.block_params(ret_block)[0];
    builder.ins().return_(&[result]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_waitid_pidfd", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}
