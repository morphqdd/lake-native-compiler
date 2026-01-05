use anyhow::Result;

use crate::compiler::{
    ctx::CompilerCtx,
    rt::rw::write::{init_write, init_write_static},
};

pub mod write;

pub fn init_rw(ctx: CompilerCtx) -> Result<CompilerCtx> {
    init_write_static(init_write(ctx)?)
}
