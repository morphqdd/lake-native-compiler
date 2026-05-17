use anyhow::Result;
use cranelift::{
    module::{Linkage, Module},
    prelude::{AbiParam, Signature},
};

use crate::compiler::ctx::CompilerCtx;

/// Declare `rt_tsc_now` as an imported function — implemented in
/// `external/${ARCH}/tsc.asm` (rdtsc on x86_64, cntvct_el0 on aarch64).
///
/// Signature: `() -> i64`.  See feature #084.
pub fn declare_tsc_now(mut ctx: CompilerCtx) -> Result<CompilerCtx> {
    let ty = ctx.module().target_config().pointer_type();
    let mut sig = Signature::new(cranelift::prelude::isa::CallConv::SystemV);
    sig.returns.push(AbiParam::new(ty));

    ctx.module_mut()
        .declare_function("rt_tsc_now", Linkage::Import, &sig)?;
    Ok(ctx)
}
