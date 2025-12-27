use std::{fs, path::Path};

use anyhow::Result;
use lake_frontend::{api::ast::Process, prelude::parse};

use crate::compiler::ctx::CompilerCtx;

mod ctx;

pub fn compile<P: AsRef<Path>>(path: P) -> Result<()> {
    let src = fs::read_to_string(&path)?;
    let ast = parse(&path, &src);
    let ctx = CompilerCtx::default();

    Ok(())
}

fn compile_machine(ctx: CompilerCtx, process: &Process<'_>) -> Result<CompilerCtx> {
    Ok(CompilerCtx)
}
