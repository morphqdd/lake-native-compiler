use std::hash::{DefaultHasher, Hash, Hasher};

use anyhow::Result;
use base64ct::{Base64, Encoding};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Type as CType, Value, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::{
    ctx::CompilerCtx,
    pipeline::expr::{BranchState, StmtOutcome, pure_expr, string_expr},
    rt::layout::ExecCtxLayout,
};

/// Compile an anonymous tuple literal `{ a b c ... }`.
///
/// Layout: heap-allocated payload of `N * 8` bytes, fat-ptr header
/// `{start, end}` produced by `rt_allocate`.  Element values are
/// folded inline within this block — each element must therefore be
/// a `pure_expr::is_pure` shape (Var, Num, Bool, Atom, arith, …).
/// Non-pure elements (e.g. `{ Foo(args) bar }`) are rejected here.
///
/// Result: the fat-ptr address (i64) is stashed into TEMP_VAL so the
/// next CPS block — typically a `let_expr` — can pick it up and bind
/// it into a variable slot.
pub fn compile(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    elems: &[Expr<'_>],
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    // All elements must be either a) pure (Num / Var / arith / atom)
    // so `fold` can resolve them inside this block, or b) a string
    // literal — strings are immutable values but their payload lives
    // in a module-level data section + fat-ptr companion that needs
    // `&mut CompilerCtx` to declare, which `fold` doesn't have.  We
    // emit the data section here and substitute the fat-ptr global
    // for that element below; everything else still runs through
    // the pure-expr fold path.
    for (i, e) in elems.iter().enumerate() {
        let ok = matches!(e, Expr::String(..)) || pure_expr::is_pure(e);
        if !ok {
            anyhow::bail!(
                "tuple element {i} is not a pure expression (use a `let` to \
                 bind the result of an effectful expression first): {:?}",
                e
            );
        }
    }

    let b = builder.create_block();
    builder.switch_to_block(b);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);

    // Lazily load vars / self_pid because elements may reference both.
    let needs_vars = elems.iter().any(|e| references_var(e));
    let vars_start = if needs_vars {
        let vars_fp = builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::VARIABLES,
        );
        Some(builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0))
    } else {
        None
    };
    let needs_self = elems.iter().any(|e| references_self(e));
    let self_pid = if needs_self {
        Some(builder.ins().load(
            ptr_ty,
            MemFlags::trusted(),
            exec_start,
            ExecCtxLayout::OWN_PID,
        ))
    } else {
        None
    };

    // Allocate the payload region (N * 8 bytes) via rt_arena_alloc
    // (#138 phase 2d).  Safe across async-spawn boundaries because
    // compile_spawn now copies buf-typed args into the spawned actor's
    // arena before transfer — the source record's lifetime can end
    // independently of the spawned reader's.
    let alloc_ref = ctx.get_func(builder, "rt_arena_alloc")?;
    let payload_size = builder.ins().iconst(ptr_ty, (elems.len() as i64) * 8);
    let call_alloc = builder.ins().call(alloc_ref, &[payload_size]);
    let fat_ptr = builder.inst_results(call_alloc)[0];
    let payload_start = builder.ins().load(ptr_ty, MemFlags::trusted(), fat_ptr, 0);

    // Fold each element into a Cranelift value and write it into its slot.
    // String literals are pure but their payload lives in a module-level
    // data section + fat-ptr companion that `fold` can't materialise on
    // its own (no &mut CompilerCtx).  Emit the data declarations here
    // and lower the element to a global_value load before storing.
    for (i, e) in elems.iter().enumerate() {
        let v = match e {
            Expr::String(s, _) => emit_string_fat_ptr(ctx, builder, ptr_ty, s)?,
            _ => pure_expr::fold_with_self(e, builder, ptr_ty, vars_start, self_pid, state),
        };
        builder
            .ins()
            .store(MemFlags::trusted(), v, payload_start, (i as i32) * 8);
    }

    // Stash the fat-ptr in TEMP_VAL so let_expr / the next CPS block can
    // pick it up.
    let store_ref = ctx.get_func(builder, "rt_store")?;
    let temp_val_offset = builder.ins().iconst(ptr_ty, ExecCtxLayout::TEMP_VAL as i64);
    let size = builder.ins().iconst(ptr_ty, 8);
    builder
        .ins()
        .call(store_ref, &[ctx_ptr, fat_ptr, size, temp_val_offset]);

    let next = builder.ins().iconst(ptr_ty, block_id + 1);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::Continue(block_id + 1))
}

fn references_var(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => false,
        Expr::Var(..) => true,
        Expr::Neg(inner) => references_var(&inner.inner),
        Expr::TupleIndex { receiver, .. } => references_var(&receiver.inner),
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r)
        | Expr::BAnd(l, r)
        | Expr::BOr(l, r)
        | Expr::BXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => references_var(&l.inner) || references_var(&r.inner),
        _ => false,
    }
}

/// Mirror `string_expr::compile`'s data-section setup but return the
/// fat-ptr's address as a Cranelift `Value` instead of stashing it in
/// `TEMP_VAL`.  Used by tuple element compilation to write a string
/// literal directly into a tuple slot.
fn emit_string_fat_ptr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ptr_ty: CType,
    s: &str,
) -> Result<Value> {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    let hash = hasher.finish();
    let encoded = Base64::encode_string(&hash.to_be_bytes());

    let bytes = string_expr::unescape(s);

    let data_id = match ctx.module().get_name(&encoded) {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            let id = ctx
                .module_mut()
                .declare_data(&encoded, Linkage::Export, false, false)?;
            let mut desc = DataDescription::new();
            desc.define(bytes.clone().into_boxed_slice());
            ctx.module_mut().define_data(id, &desc)?;
            id
        }
    };

    let fat_ptr_name = format!("fp_{encoded}");
    let fat_ptr_id = match ctx.module().get_name(&fat_ptr_name) {
        Some(FuncOrDataId::Data(id)) => id,
        _ => {
            let id = ctx
                .module_mut()
                .declare_data(&fat_ptr_name, Linkage::Export, true, false)?;
            let mut desc = DataDescription::new();
            desc.define_zeroinit(16);
            ctx.module_mut().define_data(id, &desc)?;
            id
        }
    };

    let data_gv = ctx.module_mut().declare_data_in_func(data_id, builder.func);
    let fat_ptr_gv = ctx
        .module_mut()
        .declare_data_in_func(fat_ptr_id, builder.func);

    let data_ptr = builder.ins().global_value(ptr_ty, data_gv);
    let fat_ptr = builder.ins().global_value(ptr_ty, fat_ptr_gv);
    let end_ptr = builder.ins().iadd_imm(data_ptr, bytes.len() as i64);

    builder.ins().store(MemFlags::new(), data_ptr, fat_ptr, 0);
    builder.ins().store(MemFlags::new(), end_ptr, fat_ptr, 8);

    Ok(fat_ptr)
}

fn references_self(expr: &Expr) -> bool {
    match expr {
        Expr::Var("self", _) => true,
        Expr::Neg(inner) => references_self(&inner.inner),
        Expr::TupleIndex { receiver, .. } => references_self(&receiver.inner),
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r)
        | Expr::BAnd(l, r)
        | Expr::BOr(l, r)
        | Expr::BXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => references_self(&l.inner) || references_self(&r.inner),
        _ => false,
    }
}
