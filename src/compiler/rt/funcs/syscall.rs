use anyhow::Result;
use cranelift::{
    module::{Linkage, Module},
    prelude::{AbiParam, Signature},
};

use crate::compiler::ctx::CompilerCtx;

/// Declare `rt_syscall` as an imported function (implemented in `external/syscall.asm`).
///
/// Signature: `(syscall_nr, a1, a2, a3, a4, a5, a6: i64) -> i64`
pub fn declare_syscall_wrapper(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let mut sig = Signature::new(cranelift::prelude::isa::CallConv::SystemV);
    for _ in 0..7 {
        sig.params.push(AbiParam::new(ty));
    }
    sig.returns.push(AbiParam::new(ty));

    ctx.module_mut()
        .declare_function("rt_syscall", Linkage::Import, &sig)?;
    Ok(ctx)
}
