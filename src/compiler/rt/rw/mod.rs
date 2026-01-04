use anyhow::Result;

use crate::compiler::{ctx::CompilerCtx, rt::rw::write::init_write};

pub mod write;

pub fn init_rw(ctx: CompilerCtx) -> Result<CompilerCtx> {
    init_write(ctx)
}
