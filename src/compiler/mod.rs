use std::{fs, path::Path, process::Command};

use anyhow::{Result, bail};
use lake_frontend::{api::ast::Process, prelude::parse};

use crate::compiler::{ctx::CompilerCtx, rt::Runtime};

mod ctx;
mod rt;

pub fn link<BP: AsRef<Path>>(build_path: BP, name: &str, bytes: &[u8]) -> Result<()> {
    fs::write(&build_path.as_ref().join(format!("{name}.o")), bytes)?;

    assert!(
        Command::new("mold")
            .args([
                "external/build/syscall.a",
                build_path
                    .as_ref()
                    .join(format!("{name}.o"))
                    .to_string_lossy()
                    .to_string()
                    .as_str(),
                "-o",
                build_path
                    .as_ref()
                    .join(name)
                    .to_string_lossy()
                    .to_string()
                    .as_str()
            ])
            .status()?
            .success()
    );
    Ok(())
}

pub fn compile<SP: AsRef<Path>>(source_path: SP) -> Result<Vec<u8>> {
    let src = fs::read_to_string(&source_path)?;
    let ast = parse(&source_path, &src);
    let mut ctx = CompilerCtx::default();

    ctx = Runtime::default().build(ctx)?;

    for machine in &ast {
        match compile_machine(ctx, machine) {
            Ok(changed_ctx) => ctx = changed_ctx,
            Err(err) => bail!(err),
        }
    }

    let obj = ctx.finish();
    let bytes = obj.emit()?;

    // let mut linker = Linker::default();
    // let finalized_bytes = linker.link(&bytes)?;
    //
    // fs::write(build_path.as_ref().join(filename), finalized_bytes)?;
    //
    // #[cfg(unix)]
    // {
    //     use std::os::unix::fs::PermissionsExt;
    //     let mut perm = fs::metadata(build_path.as_ref().join(filename))?.permissions();
    //     perm.set_mode(0o755);
    //     fs::set_permissions(build_path.as_ref().join(filename), perm)?;
    // }
    Ok(bytes)
}

fn compile_machine(ctx: CompilerCtx, _machine: &Process<'_>) -> Result<CompilerCtx> {
    Ok(ctx)
}

#[cfg(test)]
mod test {
    use std::{
        fs,
        process::{Command, ExitStatus},
    };

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::compiler::{compile, link};

    #[test]
    fn compile_simple_program() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path();
        let content = "main is { n i32.1 -> { n } }";
        fs::write(path.join("main.lake"), &content)?;
        let bytes = compile(path.join("main.lake"))?;
        link(dir.path(), "main", &bytes)?;
        let prog = Command::new(path.join("main")).status();
        assert_eq!(prog?.code(), Some(0));
        Ok(())
    }
}
