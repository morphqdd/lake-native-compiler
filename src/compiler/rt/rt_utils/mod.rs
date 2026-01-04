use anyhow::Result;
use cranelift::{
    module::{Linkage, Module},
    prelude::AbiParam,
};

use crate::compiler::ctx::CompilerCtx;
pub mod exit;

pub fn init_syscall_wrapper(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let target_pointer_ty = ctx.module().target_config().pointer_type();
    let mut syscall_sig = ctx.module().make_signature();
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));
    syscall_sig.params.push(AbiParam::new(target_pointer_ty));

    syscall_sig.returns.push(AbiParam::new(target_pointer_ty));

    ctx.module_mut()
        .declare_function("rt_syscall", Linkage::Import, &syscall_sig)?;

    Ok(ctx)
}
