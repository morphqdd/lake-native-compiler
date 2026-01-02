use std::{fs, path::Path, process::Command};

use anyhow::Result;
use lake_native_compiler::compiler::{compile, link};

fn main() -> Result<()> {
    let obj_bytes = compile(Path::new("examples/simple/simple.lake"))?;

    link("examples/simple/build", "simple", &obj_bytes)?;

    Ok(())
}
