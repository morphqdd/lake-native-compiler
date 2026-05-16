//! `rt_die_actor()` — mark the currently running actor for death.
//!
//! Sets `exec_ctx.IS_DYING = 1` on the current actor's exec_ctx, so the
//! next pass through `machine.rs::quantum_loop_block` returns
//! STOP_DONE and the scheduler unlinks the actor.  When lakec was
//! invoked with `LAKE_DEATH_LOG=1` the helper also emits a stderr
//! diagnostic.
//!
//! Cross-references: the same logic is inlined into `rt_allocate`'s
//! OOM branch for the legacy (pre-tuple-ABI) call sites.  The
//! standalone function exists so lowering's bare-fallible-call wrapper
//! can invoke "die" without re-emitting the scheduler-global walk.
//!
//! Fallback path: when called before the scheduler context is
//! initialised (or before any actor is registered) the helper exits
//! the program with code 137 — the "128 + SIGKILL" Unix convention.

use anyhow::{Result, anyhow};
use cranelift::{
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, TrapCode,
    },
};

use crate::compiler::{
    ctx::CompilerCtx,
    rt::layout::{
        ExecCtxLayout, process_ctx::ProcessCtxLayout,
        sheduler_ctx::ShedulerCtxLayout,
    },
    target::LinuxSyscalls,
};

pub fn define_die_actor(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_die_actor")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    // Signature: () -> ()  — no arguments, no return value.  Callers
    // ignore the "return".
    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    // Locate the scheduler global.  The 16-byte fat-ptr slot's first
    // 8 bytes are `sched_ctx.start`; zero means scheduler ctx is not
    // yet initialised → fall through to the process-exit path.
    let sched_data_id = match ctx.module().get_name("sheduler_ctx_fat_ptr") {
        Some(FuncOrDataId::Data(id)) => id,
        _ => return Err(anyhow!("sheduler_ctx_fat_ptr global missing for rt_die_actor")),
    };
    let sched_gv = ctx
        .module_mut()
        .declare_data_in_func(sched_data_id, &mut builder.func);
    let sched_fat_addr = builder.ins().global_value(ty, sched_gv);
    let sched_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), sched_fat_addr, 0);

    let check_count_block = builder.create_block();
    let init_exit_block = builder.create_block();
    let mark_actor_block = builder.create_block();

    let sched_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, sched_ptr, 0);
    builder
        .ins()
        .brif(sched_nonzero, check_count_block, &[], init_exit_block, &[]);

    builder.switch_to_block(check_count_block);
    builder.seal_block(check_count_block);
    let real_count = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::REAL_COUNT_OF_PROCESSES,
    );
    let count_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, real_count, 0);
    builder
        .ins()
        .brif(count_nonzero, mark_actor_block, &[], init_exit_block, &[]);

    builder.switch_to_block(mark_actor_block);
    builder.seal_block(mark_actor_block);
    let proc_arr_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::PROCESS_ARR_FAT,
    );
    let proc_arr_start = builder
        .ins()
        .load(ty, MemFlags::trusted(), proc_arr_fat, 0);
    let current_idx = builder.ins().load(
        ty,
        MemFlags::trusted(),
        sched_ptr,
        ShedulerCtxLayout::CURRENT_PROCESS,
    );
    let idx_scaled = builder.ins().imul_imm(current_idx, 8);
    let slot_addr = builder.ins().iadd(proc_arr_start, idx_scaled);
    let proc_ctx_fat = builder.ins().load(ty, MemFlags::trusted(), slot_addr, 0);
    let proc_ctx_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), proc_ctx_fat, 0);
    let exec_ctx_fat = builder.ins().load(
        ty,
        MemFlags::trusted(),
        proc_ctx_ptr,
        ProcessCtxLayout::EXEC_CTX,
    );
    let exec_ctx_ptr = builder
        .ins()
        .load(ty, MemFlags::trusted(), exec_ctx_fat, 0);
    let one = builder.ins().iconst(ty, 1);
    builder.ins().store(
        MemFlags::trusted(),
        one,
        exec_ctx_ptr,
        ExecCtxLayout::IS_DYING,
    );

    let want_log = std::env::var("LAKE_DEATH_LOG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if want_log {
        let syscall_ref = ctx
            .module_mut()
            .declare_func_in_func(syscall_id, &mut builder.func);
        const MSG: &str = "lake: actor died — fallible rt-call rejected\n";
        let msg_data_id = ctx
            .module_mut()
            .declare_data("__lake_die_actor_msg", Linkage::Local, false, false)?;
        let mut desc = DataDescription::new();
        desc.define(MSG.as_bytes().to_vec().into_boxed_slice());
        ctx.module_mut().define_data(msg_data_id, &desc)?;
        let msg_gv = ctx
            .module_mut()
            .declare_data_in_func(msg_data_id, &mut builder.func);
        let msg_ptr = builder.ins().global_value(ty, msg_gv);
        let msg_len = builder.ins().iconst(ty, MSG.len() as i64);
        let sys_write = builder.ins().iconst(ty, LinuxSyscalls::for_host().sys_write);
        let stderr_fd = builder.ins().iconst(ty, 2);
        let zero_arg = builder.ins().iconst(ty, 0);
        builder.ins().call(
            syscall_ref,
            &[sys_write, stderr_fd, msg_ptr, msg_len, zero_arg, zero_arg, zero_arg],
        );
    }
    builder.ins().return_(&[]);

    // ── init_exit: no actor yet — exit process cleanly with diagnostic.
    //    Init-time failures are rare and unrecoverable; always log so
    //    "exit 137 with no message" never happens in the wild.  The
    //    LAKE_DEATH_LOG knob only gates the per-actor crash log, which
    //    can be noisy on workloads that tolerate occasional death.
    builder.switch_to_block(init_exit_block);
    builder.seal_block(init_exit_block);
    let syscall_ref_exit = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);
    const MSG_INIT: &str =
        "lake: init failed — rt-fn aborted before scheduler ready (likely io_uring_setup or early rt_allocate)\n";
    let msg_init_id =
        ctx.module_mut()
            .declare_data("__lake_die_actor_init_msg", Linkage::Local, false, false)?;
    let mut desc = DataDescription::new();
    desc.define(MSG_INIT.as_bytes().to_vec().into_boxed_slice());
    ctx.module_mut().define_data(msg_init_id, &desc)?;
    let msg_gv = ctx
        .module_mut()
        .declare_data_in_func(msg_init_id, &mut builder.func);
    let msg_ptr = builder.ins().global_value(ty, msg_gv);
    let msg_len = builder.ins().iconst(ty, MSG_INIT.len() as i64);
    let sys_write = builder.ins().iconst(ty, LinuxSyscalls::for_host().sys_write);
    let stderr_fd = builder.ins().iconst(ty, 2);
    let zero_arg = builder.ins().iconst(ty, 0);
    builder.ins().call(
        syscall_ref_exit,
        &[sys_write, stderr_fd, msg_ptr, msg_len, zero_arg, zero_arg, zero_arg],
    );
    let sys_exit = builder.ins().iconst(ty, LinuxSyscalls::for_host().sys_exit);
    let code = builder.ins().iconst(ty, 137);
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().call(
        syscall_ref_exit,
        &[sys_exit, code, zero, zero, zero, zero, zero],
    );
    builder.ins().trap(TrapCode::user(0xDE).unwrap());

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_die_actor", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
