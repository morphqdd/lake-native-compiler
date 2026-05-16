// Process environment helpers — argv / envp access from Lake.
//
// The asm shim in `external/entry.asm` populates `lake_argc` /
// `lake_argv` / `lake_envp` and exposes:
//
//   rt_argc_raw()  -> i64           // process argc
//   rt_argv_raw()  -> i64           // &argv[0]   (array of cstr ptrs)
//   rt_envp_raw()  -> i64           // &envp[0]   (array of cstr ptrs)
//   rt_cstr_len(p) -> i64           // strlen for a null-terminated C string
//
// These return raw kernel-supplied pointers as `i64`.  Lake's surface
// types cannot hold raw pointers safely, so this module defines two
// thin wrappers used by `std/env.lake` to materialise the data:
//
//   rt_load_ptr_raw(p, i) -> i64    // *((i64*)p + i)
//   rt_cstr_to_buf(p)     -> {atom buf}
//
// `rt_cstr_to_buf` allocates a fresh buf sized to `strlen(p)`, copies
// the bytes, and returns it wrapped in `{:ok buf}` (or `{:err :nil}`
// when `p == 0`).  Together with `rt_argc_raw` and `rt_load_ptr_raw`
// this is enough for the Lake-side `argv` / `envp` walks.

use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::BlockArg,
    module::{FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, IntCC, MemFlags, Signature,
        isa::CallConv,
    },
};

use crate::compiler::{ctx::CompilerCtx, pipeline::expr::pure_expr::atom_id};

/// Build `cmp_str_buf(s: str, b: buf) -> i64` — byte-equality
/// check between a Lake `str` literal and a `buf`.  Returns 1 on
/// exact match (lengths equal AND all bytes equal), 0 otherwise.
///
/// Exists because Lake's type system distinguishes `str` from `buf`
/// (see #45), so the existing `rt_load_u*` / `rt_copy_bytes`
/// helpers cannot be called with mismatched types from Lake source
/// — and CLI argv-comparison against compile-time command names
/// would otherwise need an alloc + copy on every check.
pub fn define_cmp_str_buf(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let s_fp = builder.block_params(entry)[0];
    let b_fp = builder.block_params(entry)[1];

    let s_start = builder.ins().load(ty, MemFlags::trusted(), s_fp, 0);
    let s_end = builder.ins().load(ty, MemFlags::trusted(), s_fp, 8);
    let s_len = builder.ins().isub(s_end, s_start);

    let b_start = builder.ins().load(ty, MemFlags::trusted(), b_fp, 0);
    let b_end = builder.ins().load(ty, MemFlags::trusted(), b_fp, 8);
    let b_len = builder.ins().isub(b_end, b_start);

    let neq_block = builder.create_block();
    let eq_len_block = builder.create_block();
    let same_len = builder.ins().icmp(IntCC::Equal, s_len, b_len);
    builder
        .ins()
        .brif(same_len, eq_len_block, &[], neq_block, &[]);

    builder.switch_to_block(eq_len_block);
    builder.seal_block(eq_len_block);
    let loop_hdr = builder.create_block();
    let loop_body = builder.create_block();
    let loop_done = builder.create_block();
    builder.append_block_param(loop_hdr, ty);
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(zero)]);

    builder.switch_to_block(loop_hdr);
    let i = builder.block_params(loop_hdr)[0];
    let done = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, i, s_len);
    builder.ins().brif(done, loop_done, &[], loop_body, &[]);

    builder.switch_to_block(loop_body);
    builder.seal_block(loop_body);
    let sa = builder.ins().iadd(s_start, i);
    let ba = builder.ins().iadd(b_start, i);
    let sv = builder.ins().uload8(ty, MemFlags::new(), sa, 0);
    let bv = builder.ins().uload8(ty, MemFlags::new(), ba, 0);
    let byte_eq = builder.ins().icmp(IntCC::Equal, sv, bv);
    let next_block = builder.create_block();
    builder.ins().brif(byte_eq, next_block, &[], neq_block, &[]);
    builder.switch_to_block(next_block);
    builder.seal_block(next_block);
    let ni = builder.ins().iadd_imm(i, 1);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(ni)]);
    builder.seal_block(loop_hdr);

    builder.switch_to_block(loop_done);
    builder.seal_block(loop_done);
    let one = builder.ins().iconst(ty, 1);
    builder.ins().return_(&[one]);

    builder.switch_to_block(neq_block);
    builder.seal_block(neq_block);
    let zr = builder.ins().iconst(ty, 0);
    builder.ins().return_(&[zr]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("cmp_str_buf", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `buf_trim(b: buf, new_len: i64) -> {}` — rewrites the buf's
/// fat-pointer `end` field to `start + new_len`.
///
/// Required for code paths that allocate via `rt_allocate(N)` and
/// then fill in fewer than `N` bytes: the allocator rounds up to a
/// power-of-two bucket size, so the raw fat-pointer's `end` reports
/// the bucket size rather than the user-visible payload size.
/// Without trimming, `size(b)` / `len(s)` read the bucket size and
/// downstream byte-equality / `rt_write` calls misbehave.
///
/// Unbounded — the caller is responsible for `new_len <=
/// bucket_size`.  Passing a larger value silently extends the
/// reported size into adjacent allocator metadata.
pub fn define_buf_trim(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let fat_ptr = builder.block_params(entry)[0];
    let new_len = builder.block_params(entry)[1];
    let start = builder.ins().load(ty, MemFlags::trusted(), fat_ptr, 0);
    let new_end = builder.ins().iadd(start, new_len);
    builder
        .ins()
        .store(MemFlags::trusted(), new_end, fat_ptr, 8);
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("buf_trim", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `buf_ptr(b: buf) -> i64` — returns the raw byte address
/// of the buf's payload (`start` field of the fat-pointer header).
///
/// Required for callers that need to pass a buf's address to a raw
/// syscall (e.g. `rt_syscall(SYS_open, buf_ptr(cstr), ...)`).  The
/// existing `rt_str_ptr` does the same job for `str`, but the type
/// system treats `buf` and `str` as distinct (see #45), so we
/// provide a separate entry point with a `buf`-typed parameter.
pub fn define_buf_ptr(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
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
    let start = builder.ins().load(ty, MemFlags::trusted(), fat_ptr, 0);
    builder.ins().return_(&[start]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("buf_ptr", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `buf_len(b: buf) -> i64` — `end - start` of the buf's
/// fat-pointer header.  `len` already implements this signature for
/// `str`; we expose a typed variant so Lake's frontend can `len()`
/// strings AND `buf_len()` byte buffers without the registry having
/// to support multiple signatures for the same name.
pub fn define_buf_len(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
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
    let start = builder.ins().load(ty, MemFlags::trusted(), fat_ptr, 0);
    let end = builder.ins().load(ty, MemFlags::trusted(), fat_ptr, 8);
    let len = builder.ins().isub(end, start);
    builder.ins().return_(&[len]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("buf_len", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Declare the three argv/envp readers + `rt_cstr_len` (implemented in
/// `external/entry.asm`).  Signature: `() -> i64` for the readers,
/// `(i64) -> i64` for `rt_cstr_len`.
pub fn declare_asm_env_helpers(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    for nullary in ["rt_argc_raw", "rt_argv_raw", "rt_envp_raw"] {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.returns.push(AbiParam::new(ty));
        ctx.module_mut()
            .declare_function(nullary, Linkage::Import, &sig)?;
    }
    let mut sig_unary = Signature::new(CallConv::SystemV);
    sig_unary.params.push(AbiParam::new(ty));
    sig_unary.returns.push(AbiParam::new(ty));
    ctx.module_mut()
        .declare_function("rt_cstr_len", Linkage::Import, &sig_unary)?;
    Ok(ctx)
}

/// Build `rt_load_ptr_raw(base: i64, index: i64) -> i64` — reads the
/// i-th `i64` slot from a raw pointer array.  Used to dereference
/// `argv[i]` / `envp[i]` whose entries are `char*` (8-byte ptrs on
/// x86-64).  No bounds checking — callers must respect `argc` /
/// envp's NULL terminator.
pub fn define_load_ptr_raw(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.params.push(AbiParam::new(ty));
    builder.func.signature.returns.push(AbiParam::new(ty));

    let entry = builder.create_block();
    builder.append_block_param(entry, ty);
    builder.append_block_param(entry, ty);
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let base = builder.block_params(entry)[0];
    let idx = builder.block_params(entry)[1];
    let off = builder.ins().imul_imm(idx, 8);
    let addr = builder.ins().iadd(base, off);
    let val = builder.ins().load(ty, MemFlags::trusted(), addr, 0);
    builder.ins().return_(&[val]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_load_ptr_raw", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}

/// Build `rt_cstr_to_buf(p: i64) -> {atom buf}`.
///
/// Returns:
///   * `{:ok buf}`     — a fresh buf with the null-terminated bytes
///                       from `p` (length = strlen(p), null not
///                       included).
///   * `{:err :nil}`   — when `p == 0` (e.g. out-of-range argv index).
///
/// Both arms allocate via `rt_allocate_raw` (16-byte tuple header).
/// The buf payload is allocated via `rt_allocate` so the user-facing
/// zero-init guarantee holds — important if a caller writes a partial
/// prefix and later reads the tail.
pub fn define_cstr_to_buf(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let alloc_raw_id = match ctx.module().get_name("rt_allocate_raw") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_allocate_raw must precede rt_cstr_to_buf")),
    };
    let alloc_id = match ctx.module().get_name("rt_allocate_raw") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_allocate_raw must precede rt_cstr_to_buf")),
    };
    let cstr_len_id = match ctx.module().get_name("rt_cstr_len") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_cstr_len must precede rt_cstr_to_buf")),
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

    let alloc_raw_ref = ctx
        .module_mut()
        .declare_func_in_func(alloc_raw_id, &mut builder.func);
    let alloc_ref = ctx
        .module_mut()
        .declare_func_in_func(alloc_id, &mut builder.func);
    let cstr_len_ref = ctx
        .module_mut()
        .declare_func_in_func(cstr_len_id, &mut builder.func);

    let p = builder.block_params(entry)[0];
    let null_block = builder.create_block();
    let ok_block = builder.create_block();
    let is_null = builder.ins().icmp_imm(IntCC::Equal, p, 0);
    builder.ins().brif(is_null, null_block, &[], ok_block, &[]);

    // ── :err :nil branch ────────────────────────────────────────────
    builder.switch_to_block(null_block);
    builder.seal_block(null_block);
    let sixteen_err = builder.ins().iconst(ty, 16);
    let call = builder.ins().call(alloc_raw_ref, &[sixteen_err]);
    let tup_fp = builder.inst_results(call)[0];
    let tup_start = builder.ins().load(ty, MemFlags::trusted(), tup_fp, 0);
    let err_a = builder.ins().iconst(ty, atom_id("err"));
    let nil_a = builder.ins().iconst(ty, atom_id("nil"));
    builder
        .ins()
        .store(MemFlags::trusted(), err_a, tup_start, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), nil_a, tup_start, 8);
    builder.ins().return_(&[tup_fp]);

    // ── :ok branch — strlen + alloc + memcpy ────────────────────────
    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    let call_len = builder.ins().call(cstr_len_ref, &[p]);
    let len = builder.inst_results(call_len)[0];

    // alloc buf of `len` bytes (zero-init via rt_allocate_raw +
    // overwrite — we'll fill every byte from the cstr).
    let call_buf = builder.ins().call(alloc_ref, &[len]);
    let buf_fp = builder.inst_results(call_buf)[0];
    let buf_start = builder.ins().load(ty, MemFlags::trusted(), buf_fp, 0);

    // Copy loop:  for (i=0; i<len; ++i) buf_start[i] = p[i];
    let loop_hdr = builder.create_block();
    let loop_body = builder.create_block();
    let loop_done = builder.create_block();
    builder.append_block_param(loop_hdr, ty);
    let zero = builder.ins().iconst(ty, 0);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(zero)]);

    builder.switch_to_block(loop_hdr);
    let i = builder.block_params(loop_hdr)[0];
    let done = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, i, len);
    builder.ins().brif(done, loop_done, &[], loop_body, &[]);

    builder.switch_to_block(loop_body);
    builder.seal_block(loop_body);
    let src_addr = builder.ins().iadd(p, i);
    let dst_addr = builder.ins().iadd(buf_start, i);
    // `uload8` zero-extends the byte to the pointer-width integer so
    // we can pass it into `istore8` (which requires a controlling
    // type ≥ i16) without a separate `uextend` instruction.
    let byte = builder.ins().uload8(ty, MemFlags::new(), src_addr, 0);
    builder.ins().istore8(MemFlags::new(), byte, dst_addr, 0);
    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().jump(loop_hdr, &[BlockArg::Value(next)]);
    builder.seal_block(loop_hdr);

    builder.switch_to_block(loop_done);
    builder.seal_block(loop_done);
    // Trim the buf's fat-pointer `end` field to `start + len` so
    // `size(b)` / `len(s)` callers see the exact cstring length
    // rather than the allocator's bucket-rounded size.  Required
    // for byte-equal argv matching (`cmp_str_buf` walks `end -
    // start`).
    let real_end = builder.ins().iadd(buf_start, len);
    builder
        .ins()
        .store(MemFlags::trusted(), real_end, buf_fp, 8);

    // Wrap {:ok buf} via rt_allocate_raw(16).  Fresh iconst — the
    // `sixteen_err` value from the :err branch does not dominate
    // this block under Cranelift's SSA rules.
    let sixteen_ok = builder.ins().iconst(ty, 16);
    let call = builder.ins().call(alloc_raw_ref, &[sixteen_ok]);
    let tup_fp = builder.inst_results(call)[0];
    let tup_start = builder.ins().load(ty, MemFlags::trusted(), tup_fp, 0);
    let ok_a = builder.ins().iconst(ty, atom_id("ok"));
    builder.ins().store(MemFlags::trusted(), ok_a, tup_start, 0);
    builder
        .ins()
        .store(MemFlags::trusted(), buf_fp, tup_start, 8);
    builder.ins().return_(&[tup_fp]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_cstr_to_buf", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);
    Ok(ctx)
}
