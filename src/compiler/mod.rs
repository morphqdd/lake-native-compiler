use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, bail};
use lake_frontend::{api::ast::Pattern, prelude::parse};

use crate::compiler::{ctx::CompilerCtx, pipeline::machine::compile_machine, rt::RuntimeBuilder};

pub mod ctx;
pub mod pipeline;
pub mod rt;

// ── Public API ────────────────────────────────────────────────────────────────

pub fn compile<SP: AsRef<Path>>(source_path: SP) -> Result<Vec<u8>> {
    let src = fs::read_to_string(&source_path)?;
    let ast = parse(&source_path, &src);

    let mut ctx = CompilerCtx::default();
    ctx = RuntimeBuilder::init(ctx)?;

    for machine in &ast {
        ctx = compile_machine(ctx, machine)?;
    }

    ctx = RuntimeBuilder::build(ctx)?;

    let obj = ctx.finish();
    Ok(obj.emit()?)
}

pub fn link<BP: AsRef<Path>>(build_path: BP, name: &str, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(&build_path)?;
    let obj_path = build_path.as_ref().join(format!("{name}.o"));
    let out_path = build_path.as_ref().join(name);
    fs::write(&obj_path, bytes)?;

    let ok = Command::new("mold")
        .args([
            "-static",
            "external/build/syscall.o",
            obj_path.to_string_lossy().as_ref(),
            "-o",
            out_path.to_string_lossy().as_ref(),
        ])
        .status()?
        .success();

    if !ok {
        bail!("mold linker failed");
    }
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Hash a branch's pattern to produce a unique u64 key and the non-default
/// parameter count.  Used by both `compile_branch` and `CompilerCtx`.
pub(crate) fn hash_pattern(patterns: &[Pattern<'_>]) -> (u64, usize) {
    let mut param_count = 0;
    let mut hasher = DefaultHasher::new();
    for p in patterns {
        if !p.has_default() {
            param_count += 1;
            p.ident().hash(&mut hasher);
            p.ty().to_string().hash(&mut hasher);
        }
    }
    (hasher.finish(), param_count)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::compiler::{compile, link};

    /// Compile `source`, link it, run it and return the exit code.
    fn compile_and_run(source: &str) -> Result<i32> {
        let dir = tempdir()?;
        let src_path = dir.path().join("prog.lake");
        fs::write(&src_path, source)?;
        let bytes = compile(&src_path)?;
        link(dir.path(), "prog", &bytes)?;
        let status = Command::new(dir.path().join("prog")).status()?;
        Ok(status.code().unwrap_or(-1))
    }

    #[test]
    fn hello_world_exits_zero() -> Result<()> {
        let src = r#"main is { n str."Hello, world!" -> { rt_write(1 n 13) } }"#;
        assert_eq!(compile_and_run(src)?, 0);
        Ok(())
    }

    #[test]
    fn num_literal_exits_zero() -> Result<()> {
        // A machine that just binds a number and does nothing else.
        let src = r#"main is { n i64.1 -> { n } }"#;
        assert_eq!(compile_and_run(src)?, 0);
        Ok(())
    }
}
