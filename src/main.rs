use std::{fs, path::Path, process::Command};

use anyhow::Result;
use lake_native_compiler::compiler::compile;

fn main() -> Result<()> {
    let obj_bytes = compile(Path::new("examples/simple/simple.lake"))?;

    fs::write(Path::new("examples/simple/build/simple.o"), obj_bytes)?;

    assert!(
        Command::new("mold")
            .args([
                "external/build/syscall.a",
                "examples/simple/build/simple.o",
                "-o",
                "examples/simple/build/simple"
            ])
            .status()?
            .success()
    );

    Ok(())
}
