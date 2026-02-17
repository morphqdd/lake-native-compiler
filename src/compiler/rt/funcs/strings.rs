use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags},
};

use crate::compiler::ctx::CompilerCtx;

/// Build `len(fat_ptr: i64) -> i64`.
///
/// Returns the number of bytes in the fat-pointer region:
///   `fat_ptr.end - fat_ptr.start`
pub fn define_len(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let fat_ptr = builder.block_params(entry)[0];
    let start = builder.ins().load(ty, MemFlags::new(), fat_ptr, 0);
    let end = builder.ins().load(ty, MemFlags::new(), fat_ptr, 8);
    let len = builder.ins().isub(end, start);
    builder.ins().return_(&[len]);

    builder.seal_all_blocks();

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("len", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `to_string_with_ln(n: i64) -> i64` (returns fat-ptr address).
///
/// Converts `n` to its decimal ASCII representation followed by `\n`,
/// allocates a buffer on the heap, and returns a fat pointer to the string.
///
/// Block layout:
/// ```text
/// entry        : allocate 32 bytes, write '\n', check is_zero / is_neg
/// zero_block   : write "0", fix fat_ptr.start, return
/// nonzero_block: compute abs(n), is_neg flag, jump to digit_loop
/// digit_loop   : (wp, val, is_neg) — write digit backward, loop
/// done_block   : (wp, is_neg) — write sign if needed, fix fat_ptr.start, return
/// ```
pub fn define_to_string_with_ln(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let alloc_id = match ctx.module().get_name("rt_allocate") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_allocate must be declared before to_string_with_ln"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    // ── Create all blocks up-front ────────────────────────────────────────────
    let entry = builder.create_block();
    let zero_block = builder.create_block();
    let nonzero_block = builder.create_block();
    let digit_loop = builder.create_block();
    let done_block = builder.create_block();

    // digit_loop params: (write_pos: i64, remaining_val: i64, is_neg: i64)
    builder.append_block_param(digit_loop, ty);
    builder.append_block_param(digit_loop, ty);
    builder.append_block_param(digit_loop, ty);

    // done_block params: (write_pos: i64, is_neg: i64)
    builder.append_block_param(done_block, ty);
    builder.append_block_param(done_block, ty);

    // ── entry ─────────────────────────────────────────────────────────────────
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let n = builder.block_params(entry)[0];

    let alloc_ref = ctx
        .module_mut()
        .declare_func_in_func(alloc_id, &mut builder.func);
    let buf_size = builder.ins().iconst(ty, 32);
    let call_alloc = builder.ins().call(alloc_ref, &[buf_size]);
    let fat_ptr = builder.inst_results(call_alloc)[0];

    // data_ptr = fat_ptr.start (pointer to the 32-byte user buffer)
    let data_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 0);

    // Always write '\n' at data_ptr[31] (last byte of the 32-byte buffer).
    let nl = builder.ins().iconst(ty, 10i64); // '\n'
    builder.ins().istore8(MemFlags::new(), nl, data_ptr, 31);

    // Start writing digits at data_ptr[30], going left.
    let wp_start = builder.ins().iadd_imm(data_ptr, 30);

    let is_zero = builder.ins().icmp_imm(IntCC::Equal, n, 0);
    builder
        .ins()
        .brif(is_zero, zero_block, &[], nonzero_block, &[]);

    // ── zero_block ────────────────────────────────────────────────────────────
    builder.switch_to_block(zero_block);
    builder.seal_block(zero_block);
    {
        let zero_ch = builder.ins().iconst(ty, 48i64); // '0'
        builder.ins().istore8(MemFlags::new(), zero_ch, wp_start, 0);
        // Update fat_ptr.start to point at '0' (data_ptr[30]).
        builder.ins().store(MemFlags::new(), wp_start, fat_ptr, 0);
        builder.ins().return_(&[fat_ptr]);
    }

    // ── nonzero_block ─────────────────────────────────────────────────────────
    builder.switch_to_block(nonzero_block);
    builder.seal_block(nonzero_block);
    {
        let is_neg = builder.ins().icmp_imm(IntCC::SignedLessThan, n, 0);
        let neg_n = builder.ins().ineg(n);
        let abs_n = builder.ins().select(is_neg, neg_n, n);
        // Extend the i8 bool to ptr_ty so it can be a block param.
        let is_neg_ext = builder.ins().uextend(ty, is_neg);
        builder.ins().jump(
            digit_loop,
            &[
                BlockArg::Value(wp_start),
                BlockArg::Value(abs_n),
                BlockArg::Value(is_neg_ext),
            ],
        );
    }

    // ── digit_loop ────────────────────────────────────────────────────────────
    // NOT sealed yet — has a back-edge from itself.
    builder.switch_to_block(digit_loop);
    {
        let wp = builder.block_params(digit_loop)[0];
        let val = builder.block_params(digit_loop)[1];
        let is_neg_flag = builder.block_params(digit_loop)[2];

        let ten = builder.ins().iconst(ty, 10i64);
        let rem = builder.ins().srem(val, ten);
        let digit = builder.ins().iadd_imm(rem, 48);
        builder.ins().istore8(MemFlags::new(), digit, wp, 0);

        let next_wp = builder.ins().iadd_imm(wp, -1);
        let next_val = builder.ins().sdiv(val, ten);

        let done = builder.ins().icmp_imm(IntCC::Equal, next_val, 0);
        builder.ins().brif(
            done,
            done_block,
            &[BlockArg::Value(next_wp), BlockArg::Value(is_neg_flag)],
            digit_loop,
            &[
                BlockArg::Value(next_wp),
                BlockArg::Value(next_val),
                BlockArg::Value(is_neg_flag),
            ],
        );
    }
    // Seal digit_loop after the back-edge is registered.
    builder.seal_block(digit_loop);

    // ── done_block ────────────────────────────────────────────────────────────
    builder.switch_to_block(done_block);
    builder.seal_block(done_block);
    {
        let wp = builder.block_params(done_block)[0];
        let is_neg_flag = builder.block_params(done_block)[1];

        // Always write '-' at wp. For positive numbers the start pointer will
        // skip past it; for negative numbers it will be included.
        let minus = builder.ins().iconst(ty, 45i64); // '-'
        builder.ins().istore8(MemFlags::new(), minus, wp, 0);

        // positive: start = wp + 1  (skip the harmless '-')
        // negative: start = wp      (include '-')
        //   => start = (wp + 1) - is_neg_flag
        let pos_start = builder.ins().iadd_imm(wp, 1);
        let start = builder.ins().isub(pos_start, is_neg_flag);

        // Update fat_ptr.start to point at the first real character.
        builder.ins().store(MemFlags::new(), start, fat_ptr, 0);
        builder.ins().return_(&[fat_ptr]);
    }

    builder.seal_all_blocks();

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("to_string_with_ln", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

pub fn define_to_string(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let alloc_id = match ctx.module().get_name("rt_allocate") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => {
            return Err(anyhow!(
                "rt_allocate must be declared before to_string_with_ln"
            ));
        }
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    // ── Create all blocks up-front ────────────────────────────────────────────
    let entry = builder.create_block();
    let zero_block = builder.create_block();
    let nonzero_block = builder.create_block();
    let digit_loop = builder.create_block();
    let done_block = builder.create_block();

    // digit_loop params: (write_pos: i64, remaining_val: i64, is_neg: i64)
    builder.append_block_param(digit_loop, ty);
    builder.append_block_param(digit_loop, ty);
    builder.append_block_param(digit_loop, ty);

    // done_block params: (write_pos: i64, is_neg: i64)
    builder.append_block_param(done_block, ty);
    builder.append_block_param(done_block, ty);

    // ── entry ─────────────────────────────────────────────────────────────────
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let n = builder.block_params(entry)[0];

    let alloc_ref = ctx
        .module_mut()
        .declare_func_in_func(alloc_id, &mut builder.func);
    let buf_size = builder.ins().iconst(ty, 31);
    let call_alloc = builder.ins().call(alloc_ref, &[buf_size]);
    let fat_ptr = builder.inst_results(call_alloc)[0];

    // data_ptr = fat_ptr.start (pointer to the 32-byte user buffer)
    let data_ptr = builder.ins().load(ty, MemFlags::new(), fat_ptr, 0);

    // Always write '\n' at data_ptr[31] (last byte of the 32-byte buffer).

    // Start writing digits at data_ptr[30], going left.
    let wp_start = builder.ins().iadd_imm(data_ptr, 30);

    let is_zero = builder.ins().icmp_imm(IntCC::Equal, n, 0);
    builder
        .ins()
        .brif(is_zero, zero_block, &[], nonzero_block, &[]);

    // ── zero_block ────────────────────────────────────────────────────────────
    builder.switch_to_block(zero_block);
    builder.seal_block(zero_block);
    {
        let zero_ch = builder.ins().iconst(ty, 48i64); // '0'
        builder.ins().istore8(MemFlags::new(), zero_ch, wp_start, 0);
        // Update fat_ptr.start to point at '0' (data_ptr[30]).
        builder.ins().store(MemFlags::new(), wp_start, fat_ptr, 0);
        builder.ins().return_(&[fat_ptr]);
    }

    // ── nonzero_block ─────────────────────────────────────────────────────────
    builder.switch_to_block(nonzero_block);
    builder.seal_block(nonzero_block);
    {
        let is_neg = builder.ins().icmp_imm(IntCC::SignedLessThan, n, 0);
        let neg_n = builder.ins().ineg(n);
        let abs_n = builder.ins().select(is_neg, neg_n, n);
        // Extend the i8 bool to ptr_ty so it can be a block param.
        let is_neg_ext = builder.ins().uextend(ty, is_neg);
        builder.ins().jump(
            digit_loop,
            &[
                BlockArg::Value(wp_start),
                BlockArg::Value(abs_n),
                BlockArg::Value(is_neg_ext),
            ],
        );
    }

    // ── digit_loop ────────────────────────────────────────────────────────────
    // NOT sealed yet — has a back-edge from itself.
    builder.switch_to_block(digit_loop);
    {
        let wp = builder.block_params(digit_loop)[0];
        let val = builder.block_params(digit_loop)[1];
        let is_neg_flag = builder.block_params(digit_loop)[2];

        let ten = builder.ins().iconst(ty, 10i64);
        let rem = builder.ins().srem(val, ten);
        let digit = builder.ins().iadd_imm(rem, 48);
        builder.ins().istore8(MemFlags::new(), digit, wp, 0);

        let next_wp = builder.ins().iadd_imm(wp, -1);
        let next_val = builder.ins().sdiv(val, ten);

        let done = builder.ins().icmp_imm(IntCC::Equal, next_val, 0);
        builder.ins().brif(
            done,
            done_block,
            &[BlockArg::Value(next_wp), BlockArg::Value(is_neg_flag)],
            digit_loop,
            &[
                BlockArg::Value(next_wp),
                BlockArg::Value(next_val),
                BlockArg::Value(is_neg_flag),
            ],
        );
    }
    // Seal digit_loop after the back-edge is registered.
    builder.seal_block(digit_loop);

    // ── done_block ────────────────────────────────────────────────────────────
    builder.switch_to_block(done_block);
    builder.seal_block(done_block);
    {
        let wp = builder.block_params(done_block)[0];
        let is_neg_flag = builder.block_params(done_block)[1];

        // Always write '-' at wp. For positive numbers the start pointer will
        // skip past it; for negative numbers it will be included.
        let minus = builder.ins().iconst(ty, 45i64); // '-'
        builder.ins().istore8(MemFlags::new(), minus, wp, 0);

        // positive: start = wp + 1  (skip the harmless '-')
        // negative: start = wp      (include '-')
        //   => start = (wp + 1) - is_neg_flag
        let pos_start = builder.ins().iadd_imm(wp, 1);
        let start = builder.ins().isub(pos_start, is_neg_flag);

        // Update fat_ptr.start to point at the first real character.
        builder.ins().store(MemFlags::new(), start, fat_ptr, 0);
        builder.ins().return_(&[fat_ptr]);
    }

    builder.seal_all_blocks();

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("to_string", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}
