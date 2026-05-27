use anyhow::{Result, anyhow};
use cranelift::{
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, MemFlags},
};

use crate::compiler::{ctx::CompilerCtx, target::LinuxSyscalls};

const PROT_READ: i64 = 0x1;
const PROT_WRITE: i64 = 0x2;
const MAP_PRIVATE: i64 = 0x02;
const MAP_ANONYMOUS: i64 = 0x20;
const MAP_NORESERVE: i64 = 0x4000;
// 4 GiB virtual reservation. Anonymous private + MAP_NORESERVE = lazy-commit:
// kernel allocates page-table entries on first touch only, so RSS stays at 0
// until actually used. Costs only address space (free on 64-bit).
const HEAP_SIZE: i64 = 4i64 * 1024 * 1024 * 1024;

/// Build `rt_mmap(size: i64) -> i64` and declare the three heap globals
/// (`heap_base`, `heap_curr`, `heap_end`).
pub fn define_mmap(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    // ── Heap globals ─────────────────────────────────────────────────────────
    for name in ["heap_base", "heap_curr", "heap_end"] {
        let id = ctx
            .module_mut()
            .declare_data(name, Linkage::Export, true, false)?;
        let mut desc = DataDescription::new();
        desc.define_zeroinit(8);
        ctx.module_mut().define_data(id, &desc)?;
    }

    // ── Free-list heads ──────────────────────────────────────────────────────
    // 21 buckets indexed by `ceil(log2(max(size, 16))) - 4`, covering payload
    // sizes 16 B..=16 MiB in powers of two.  Each entry is a fat-pointer
    // address (or 0 = empty) — pop/push at the head of an intrusive linked
    // list whose `next` link is stored at offset 0 of the freed block's
    // payload.  Allocations larger than 16 MiB take the direct-mmap path:
    // they bypass the bump heap entirely and are unmapped on free, so they
    // never leak.
    let free_list_id =
        ctx.module_mut()
            .declare_data("free_list_heads", Linkage::Export, true, false)?;
    let mut free_list_desc = DataDescription::new();
    free_list_desc.define_zeroinit(21 * 8);
    ctx.module_mut()
        .define_data(free_list_id, &free_list_desc)?;

    // ── Slab class state table (#150 phase 2) ────────────────────────────
    // 21 classes × 24 B per class (slabs_head, current_slab, padding).
    // Used by `rt_allocate_slab` / `rt_free_slab` — see SlabLayout in
    // `rt/layout/slab.rs`.  Zero-init: empty class = both pointers null.
    let class_state_id = ctx.module_mut().declare_data(
        "slab_class_state",
        Linkage::Export,
        true,
        false,
    )?;
    let mut class_state_desc = DataDescription::new();
    use crate::compiler::rt::layout::slab::SlabLayout;
    class_state_desc.define_zeroinit(
        (SlabLayout::NUM_CLASSES as usize) * (SlabLayout::CLASS_STATE_SIZE as usize),
    );
    ctx.module_mut()
        .define_data(class_state_id, &class_state_desc)?;

    // ── rt_mmap function ──────────────────────────────────────────────────────
    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_mmap")),
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

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let length = builder.block_params(entry)[0];
    let sys_mmap = builder.ins().iconst(ty, ctx.syscalls().sys_mmap);
    let addr = builder.ins().iconst(ty, 0);
    let prot = builder.ins().iconst(ty, PROT_READ | PROT_WRITE);
    let flags = builder
        .ins()
        .iconst(ty, MAP_ANONYMOUS | MAP_PRIVATE | MAP_NORESERVE);
    let fd = builder.ins().iconst(ty, -1i64);
    let offset = builder.ins().iconst(ty, 0);

    let call = builder.ins().call(
        syscall_ref,
        &[sys_mmap, addr, length, prot, flags, fd, offset],
    );
    let mapped_ptr = builder.inst_results(call)[0];
    builder.ins().return_(&[mapped_ptr]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_mmap", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_munmap(addr: i64, size: i64)` — releases an mmap'd region.
///
/// Used by the huge-allocation path in `rt_free` to return oversized
/// (> MAX_BUCKET_SIZE) payloads to the kernel.  The fat-pointer header
/// itself lives in the regular bump heap and is recycled separately.
pub fn define_munmap(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let syscall_id = match ctx.module().get_name("rt_syscall") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_syscall must be declared before rt_munmap")),
    };

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

    let syscall_ref = ctx
        .module_mut()
        .declare_func_in_func(syscall_id, &mut builder.func);

    let addr = builder.block_params(entry)[0];
    let length = builder.block_params(entry)[1];
    let sys_munmap = builder
        .ins()
        .iconst(ty, ctx.syscalls().sys_munmap);
    let zero = builder.ins().iconst(ty, 0);

    // rt_syscall takes 7 fixed args; munmap uses only nr+addr+length.
    let _ = builder.ins().call(
        syscall_ref,
        &[sys_munmap, addr, length, zero, zero, zero, zero],
    );
    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_munmap", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

/// Build `rt_init_heap()` — calls `rt_mmap(HEAP_SIZE)` once and writes the
/// result into the three heap globals (`heap_base`, `heap_curr`, `heap_end`).
///
/// Must be called at the very start of `_start`, before any allocation.
pub fn define_init_heap(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();

    let (heap_base_id, heap_curr_id, heap_end_id) = match (
        ctx.module().get_name("heap_base"),
        ctx.module().get_name("heap_curr"),
        ctx.module().get_name("heap_end"),
    ) {
        (Some(FuncOrDataId::Data(b)), Some(FuncOrDataId::Data(c)), Some(FuncOrDataId::Data(e))) => {
            (b, c, e)
        }
        _ => return Err(anyhow!("Heap globals must be declared before rt_init_heap")),
    };

    let mmap_id = match ctx.module().get_name("rt_mmap") {
        Some(FuncOrDataId::Func(id)) => id,
        _ => return Err(anyhow!("rt_mmap must be declared before rt_init_heap")),
    };

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut module_ctx = ctx.module().make_context();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    let entry = builder.create_block();
    builder.switch_to_block(entry);
    builder.seal_block(entry);

    let mmap_ref = ctx
        .module_mut()
        .declare_func_in_func(mmap_id, &mut builder.func);

    let heap_base_gv = ctx
        .module_mut()
        .declare_data_in_func(heap_base_id, &mut builder.func);
    let heap_curr_gv = ctx
        .module_mut()
        .declare_data_in_func(heap_curr_id, &mut builder.func);
    let heap_end_gv = ctx
        .module_mut()
        .declare_data_in_func(heap_end_id, &mut builder.func);

    let heap_base_ptr = builder.ins().global_value(ty, heap_base_gv);
    let heap_curr_ptr = builder.ins().global_value(ty, heap_curr_gv);
    let heap_end_ptr = builder.ins().global_value(ty, heap_end_gv);

    // base = rt_mmap(HEAP_SIZE)
    let size = builder.ins().iconst(ty, HEAP_SIZE);
    let call = builder.ins().call(mmap_ref, &[size]);
    let base = builder.inst_results(call)[0];

    let end = builder.ins().iadd_imm(base, HEAP_SIZE);

    builder.ins().store(MemFlags::new(), base, heap_base_ptr, 0);
    builder.ins().store(MemFlags::new(), base, heap_curr_ptr, 0);
    builder.ins().store(MemFlags::new(), end, heap_end_ptr, 0);

    builder.ins().return_(&[]);

    let sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function("rt_init_heap", Linkage::Export, &sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}
