use anyhow::{Result, bail};
use cranelift::{
    frontend::Switch,
    module::{Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder},
};
use lake_frontend::api::ast::{Machine, MachineItem};
use log::{debug, trace};

use crate::compiler::{
    ctx::CompilerCtx, pipeline::branch::compile_branch, rt::layout::ExecCtxLayout,
};

/// Compile a single Lake machine to a Cranelift function.
///
/// The generated function signature is `fn(ctx_fat_ptr: i64) -> i64` where the
/// return value is the next block_id, or -1 when the branch is done.
pub fn compile_machine(ctx: &mut CompilerCtx, machine: &Machine<'_>) -> Result<()> {
    let machine_ident = machine.ident.to_string();
    debug!("  branches: {}", machine.items.len());
    ctx.add_machine(&machine_ident);

    let ptr_ty = ctx.module().target_config().pointer_type();
    let mut module_ctx = ctx.module().make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);
    let rt_funcs = ctx.rt_funcs().clone();
    ctx.begin_function();

    // Signature: (ctx_fat_ptr: ptr) -> (next_block_id: ptr)
    builder.func.signature.params.push(AbiParam::new(ptr_ty));
    builder.func.signature.returns.push(AbiParam::new(ptr_ty));

    // ── Entry block ───────────────────────────────────────────────────────────
    let entry = builder.create_block();
    let default_block = builder.create_block();
    let machine_switch_block = builder.create_block();

    builder.append_block_param(entry, ptr_ty);
    builder.switch_to_block(entry);
    let ctx_fat_ptr = builder.block_params(entry)[0];

    // Keep the fat-ptr in a variable so inner blocks can reload it.
    let machine_ctx_var = builder.declare_var(ptr_ty);
    builder.def_var(machine_ctx_var, ctx_fat_ptr);
    builder.ins().jump(machine_switch_block, &[]);

    // ── Compile branches ──────────────────────────────────────────────────────
    let mut machine_switch = Switch::new();
    for (branch_id, item) in machine.items.iter().enumerate() {
        let MachineItem::Branch(ref branch) = item.inner else {
            bail!("Except branch, but found: {:?}", item);
        };

        compile_branch(
            ctx,
            &mut builder,
            &machine_ident,
            &mut machine_switch,
            branch_id as u128,
            branch,
            machine_ctx_var,
        )?;
    }

    // ── Machine-level switch: dispatch on BRANCH_ID ───────────────────────────
    builder.switch_to_block(machine_switch_block);
    let ctx_fat_ptr = builder.use_var(machine_ctx_var);

    // Use rt_load_u64 so bounds-checking is applied.
    // let load_u64_ref = ctx.get_func(&mut builder, "rt_load_u64")?;
    let load_u64_ref = rt_funcs.load_u64_ref(ctx.module_mut(), &mut builder);
    let branch_id_offset = builder
        .ins()
        .iconst(ptr_ty, ExecCtxLayout::BRANCH_ID as i64);
    let call = builder
        .ins()
        .call(load_u64_ref, &[ctx_fat_ptr, branch_id_offset]);
    let branch_id = builder.inst_results(call)[0];
    machine_switch.emit(&mut builder, branch_id, default_block);

    // ── Default block: return -1 (no matching branch) ────────────────────────
    builder.switch_to_block(default_block);
    let neg = builder.ins().iconst(ptr_ty, -1);
    builder.ins().return_(&[neg]);

    builder.seal_all_blocks();

    trace!("CLIF [{}]:\n{}", machine_ident, builder.func);

    // ── Emit the function ─────────────────────────────────────────────────────
    let machine_sig = builder.func.signature.clone();
    let id = ctx
        .module_mut()
        .declare_function(&machine_ident, Linkage::Export, &machine_sig)?;

    ctx.module_mut().define_function(id, &mut module_ctx)?;
    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(())
}
