use anyhow::Result;
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Variable},
};
use lake_frontend::api::{
    ast::{Branch, Clean, Ident, Pattern, Type},
    expr::Expr,
};

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, compile_expr, pure_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile a `wait { handler* }` expression.
///
/// ## Single handler (no guards)
///
/// ```text
/// Block N (wait_check, in branch_switch):
///   HEAD != TAIL → dequeue_block
///   HEAD == TAIL → suspend_block
///
/// dequeue_block:
///   copy mailbox[HEAD..HEAD+N] → VARIABLES[base..]
///   advance HEAD by N (mod 256)
///   jump quantum_continue(body_start)
///
/// suspend_block:
///   BLOCK_ID = N  (resume here on wake)
///   jump quantum_continue(STOP_WAIT)
/// ```
///
/// ## Multiple handlers with i64 literal guards
///
/// ```text
/// Block N (wait_check, in branch_switch):
///   HEAD != TAIL → dequeue_block
///   HEAD == TAIL → suspend_block
///
/// dequeue_block:
///   discriminant = mailbox[HEAD * 8]
///   copy mailbox[HEAD..HEAD+arg_count] → VARIABLES[base..]
///   advance HEAD by arg_count
///   Switch(discriminant):
///     guard_i → b_jump_i → quantum_continue(handler_i_body)
///     default → b_jump_wildcard → quantum_continue(wildcard_body)
///
/// suspend_block:
///   BLOCK_ID = N
///   jump quantum_continue(STOP_WAIT)
/// ```
pub(crate) fn compile<'a>(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder<'_>,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    collect: Vec<Branch<'_>>,
    filter: Vec<Expr<'a>>,
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let rt_funcs = ctx.rt_funcs().clone();
    let qb = ctx.quantum_block();

    anyhow::ensure!(
        !collect.is_empty(),
        "wait expression must have at least one handler"
    );

    // ── Per-handler metadata ─────────────────────────────────────────────────
    struct HandlerMeta {
        arg_count: usize,
        var_base: usize,
        guard_i64: Option<i64>,   // None = wildcard (default case)
    }

    let handler_var_base_start = state.len();
    let mut handlers: Vec<HandlerMeta> = Vec::with_capacity(collect.len());

    for handler in &collect {
        let patterns = Clean::<Vec<Pattern<'_>>>::clean(handler);
        let guard = patterns.first().and_then(|p| p.guard_i64());
        let arg_count = patterns
            .iter()
            .filter(|p| !p.is_wildcard() && !p.is_literal_guard())
            .count();

        // Only register variables for the first wildcard/binding handler
        // (all handlers share the same slots since only one fires per activation)
        let var_base = if handlers.iter().any(|h| h.guard_i64.is_none()) {
            // A previous wildcard handler already registered the slots
            handler_var_base_start
        } else if guard.is_none() {
            // This is the wildcard handler — register its variables
            let base = state.len();
            for p in &patterns {
                if p.is_wildcard() || p.is_literal_guard() {
                    continue;
                }
                let ident_str = Clean::<Ident<'_>>::clean(p).to_string();
                let lake_ty = Clean::<Type<'_>>::clean(p).to_string();
                state.insert_with_lake_type(ident_str, ptr_ty, lake_ty);
            }
            base
        } else {
            handler_var_base_start
        };

        handlers.push(HandlerMeta { arg_count, var_base, guard_i64: guard });
    }

    // arg_count used for dequeue = largest across all handlers
    let dequeue_arg_count = handlers.iter().map(|h| h.arg_count).max().unwrap_or(0);
    let has_guards = handlers.iter().any(|h| h.guard_i64.is_some());

    // ── Compile all handler bodies ───────────────────────────────────────────
    // Assign body start block IDs sequentially starting at block_id + 1.
    let mut body_starts: Vec<i64> = Vec::with_capacity(collect.len());
    let mut current_id = block_id + 1;

    for handler in &collect {
        body_starts.push(current_id);
        for expr in &handler.body {
            match compile_expr(
                ctx,
                builder,
                machine_ctx_var,
                current_id,
                branch_switch,
                state,
                &expr.inner,
            )? {
                StmtOutcome::Continue(id) => current_id = id,
                outcome => {
                    current_id = outcome.next_available();
                    break;
                }
            }
        }
    }

    let after_wait_id = current_id;

    // ── Internal Cranelift blocks ────────────────────────────────────────────
    let wait_check_block = builder.create_block();
    let dequeue_block = builder.create_block();
    let suspend_block = builder.create_block();
    // Reached when a filter is present and the dequeued message's first
    // arg (sender pid) doesn't match any filter pid.  Drops the
    // message (HEAD has already advanced) and re-enters wait_check
    // so the next message can be tried.  This is "drop on miss" — a
    // simpler semantics than full Erlang-style selective receive,
    // sufficient for the sync-return convention where the caller
    // expects only one specific reply while in the wait.
    let drop_block = builder.create_block();
    // Block where dispatch happens after filter (if any) succeeds.
    let dispatch_block = builder.create_block();

    // Per-handler jump trampoline blocks (needed because Switch targets must be
    // Cranelift blocks, not block IDs in the quantum loop).
    let jump_blocks: Vec<_> = (0..collect.len()).map(|_| builder.create_block()).collect();
    let no_match_block = builder.create_block(); // only used when no wildcard handler

    // ── wait_check_block ─────────────────────────────────────────────────────
    builder.switch_to_block(wait_check_block);
    {
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        let head = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_HEAD);
        let tail = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_TAIL);
        let has_msg = builder.ins().icmp(IntCC::NotEqual, head, tail);
        builder.ins().brif(has_msg, dequeue_block, &[], suspend_block, &[]);
    }

    // dispatch_block takes the message's first arg as a block parameter
    // when one is available — this avoids reaching across Rust block
    // scopes for the Cranelift `Value`.  When dequeue produces no
    // discriminant we pass a zero placeholder which dispatch_block
    // ignores because `has_guards` will be false in that case.
    builder.append_block_param(dispatch_block, ptr_ty);

    // ── dequeue_block ────────────────────────────────────────────────────────
    builder.switch_to_block(dequeue_block);
    let mut discriminant: Option<cranelift::prelude::Value> = None;
    {
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

        let mailbox_fat = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_FAT);
        let head = ExecCtxLayout::load(builder, ptr_ty, exec_start, ExecCtxLayout::MAILBOX_HEAD);
        // Inlined fat-ptr deref: scheduler-owned mailbox, no bounds check.
        let mailbox_start = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), mailbox_fat, 0);

        // Load discriminant (first mailbox arg) — needed both for guard
        // dispatch and for the optional sender-pid filter check below.
        discriminant = if (has_guards || !filter.is_empty()) && dequeue_arg_count > 0 {
            let msg_index_mod = builder.ins().band_imm(head, 255);
            let msg_offset = builder.ins().imul_imm(msg_index_mod, 8);
            let addr = builder.ins().iadd(mailbox_start, msg_offset);
            Some(
                builder
                    .ins()
                    .load(ptr_ty, MemFlags::trusted(), addr, 0),
            )
        } else {
            None
        };

        // Dequeue args into VARIABLES (inlined deref).
        let vars_fat = ExecCtxLayout::load(
            builder,
            ptr_ty,
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        let vars_ptr = builder
            .ins()
            .load(ptr_ty, MemFlags::trusted(), vars_fat, 0);

        // Use the wildcard handler's var_base for dequeue destination
        let dequeue_var_base = handlers
            .iter()
            .find(|h| h.guard_i64.is_none())
            .map(|h| h.var_base)
            .unwrap_or(handler_var_base_start);

        for i in 0..dequeue_arg_count {
            let msg_index = builder.ins().iadd_imm(head, i as i64);
            let msg_index_mod = builder.ins().band_imm(msg_index, 255);
            let msg_offset = builder.ins().imul_imm(msg_index_mod, 8);
            let src_addr = builder.ins().iadd(mailbox_start, msg_offset);
            let msg_val = builder
                .ins()
                .load(ptr_ty, MemFlags::trusted(), src_addr, 0);

            builder.ins().store(
                MemFlags::trusted(),
                msg_val,
                vars_ptr,
                (dequeue_var_base + i) as i32 * 8,
            );
        }
        let _ = rt_funcs;

        // Advance HEAD
        let new_head = builder.ins().iadd_imm(head, dequeue_arg_count as i64);
        let new_head_mod = builder.ins().band_imm(new_head, 255);
        ExecCtxLayout::store(builder, new_head_mod, exec_start, ExecCtxLayout::MAILBOX_HEAD);

        // Discriminant value passed to dispatch_block.  Zero placeholder
        // when no discriminant was loaded (no guards, no filter).
        let disc_for_dispatch = discriminant
            .unwrap_or_else(|| builder.ins().iconst(ptr_ty, 0));

        // Filter check (only when `wait <pid>* { ... }` had filter pids).
        // For each filter expression, fold its value at runtime and
        // compare against the message's first arg (the sender pid by
        // convention).  Any match → dispatch; no match → drop_block.
        if !filter.is_empty() {
            let disc = discriminant.expect("filter requires a non-empty message");
            // `vars_ptr` is already the dereferenced START of the variables
            // buffer (we inlined the fat-ptr deref above, replacing the
            // rt_load_u64 call).  Pass it through to fold as-is — an extra
            // load(vars_ptr, 0) here would read slot 0 instead of the
            // fat-ptr start and segfault.
            let vars_ptr_for_filter = vars_ptr;
            // OR-chain: each check goes to dispatch_block on hit, falls
            // through to next check on miss.  Final miss → drop_block.
            for (idx, filter_expr) in filter.iter().enumerate() {
                let next_check_block = if idx + 1 == filter.len() {
                    drop_block
                } else {
                    builder.create_block()
                };
                let pid_val = pure_expr::fold(
                    filter_expr,
                    builder,
                    ptr_ty,
                    Some(vars_ptr_for_filter),
                    state,
                );
                let eq = builder.ins().icmp(IntCC::Equal, disc, pid_val);
                builder.ins().brif(
                    eq,
                    dispatch_block,
                    &[BlockArg::Value(disc_for_dispatch)],
                    next_check_block,
                    &[],
                );
                if next_check_block != drop_block {
                    builder.switch_to_block(next_check_block);
                }
            }
        } else {
            builder
                .ins()
                .jump(dispatch_block, &[BlockArg::Value(disc_for_dispatch)]);
        }
    }

    // ── dispatch_block ────────────────────────────────────────────────────
    builder.switch_to_block(dispatch_block);
    {
        // Discriminant arrives as a block parameter from dequeue_block (or
        // from any of the filter chain's success branches).
        let disc = builder.block_params(dispatch_block)[0];

        // Dispatch
        if has_guards {
            let mut guard_switch = Switch::new();
            let mut wildcard_block = no_match_block;

            for (i, meta) in handlers.iter().enumerate() {
                if let Some(v) = meta.guard_i64 {
                    guard_switch.set_entry(v as u128, jump_blocks[i]);
                } else {
                    wildcard_block = jump_blocks[i];
                }
            }
            guard_switch.emit(builder, disc, wildcard_block);
        } else {
            // Single handler or all wildcards: jump directly
            let start_val = builder.ins().iconst(ptr_ty, body_starts[0]);
            builder.ins().jump(qb, &[BlockArg::Value(start_val)]);
        }
    }

    // ── Per-handler jump trampoline blocks ───────────────────────────────────
    for (i, &jb) in jump_blocks.iter().enumerate() {
        builder.switch_to_block(jb);
        let v = builder.ins().iconst(ptr_ty, body_starts[i]);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
    }

    // ── no_match_block: should not be reached if messages are well-typed ─────
    builder.switch_to_block(no_match_block);
    {
        // Silently skip: re-enter wait_check on next quantum
        let v = builder.ins().iconst(ptr_ty, block_id);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
    }

    // ── drop_block: filter rejected the message ──────────────────────────
    // The message has already been removed from the mailbox (HEAD was
    // advanced before the filter check).  Re-enter wait_check so the
    // next message — if any — gets the same treatment.  This is
    // "drop on miss"; full Erlang-style selective receive (where
    // non-matching messages stay in the mailbox for later) is future
    // work and would require a more elaborate mailbox layout.
    builder.switch_to_block(drop_block);
    {
        let v = builder.ins().iconst(ptr_ty, block_id);
        builder.ins().jump(qb, &[BlockArg::Value(v)]);
    }

    // ── suspend_block ────────────────────────────────────────────────────────
    builder.switch_to_block(suspend_block);
    {
        let ctx_ptr = builder.use_var(machine_ctx_var);
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
        let block_id_val = builder.ins().iconst(ptr_ty, block_id);
        ExecCtxLayout::store(builder, block_id_val, exec_start, ExecCtxLayout::BLOCK_ID);
        let wait_stop = builder.ins().iconst(ptr_ty, -3i64);
        builder.ins().jump(qb, &[BlockArg::Value(wait_stop)]);
    }

    // Register wait_check in the branch switch
    branch_switch.set_entry(block_id as u128, wait_check_block);

    Ok(StmtOutcome::Wait { next_avaitlable: after_wait_id })
}
