use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, bail};
use log::{debug, info};
use lake_frontend::{
    api::{
        ast::{Clean, Ident, Pattern, Type},
        expr::Expr,
    },
    prelude::parse,
};

use crate::compiler::{ctx::CompilerCtx, pipeline::machine::compile_machine, rt::RuntimeBuilder};

pub mod ctx;
pub mod pipeline;
pub mod rt;

pub fn compile<SP: AsRef<Path>>(source_path: SP) -> Result<Vec<u8>> {
    let path = source_path.as_ref();
    info!("compile: {}", path.display());

    let src = fs::read_to_string(path)?;
    let ast = parse(path, &src)?;
    info!("parsed {} top-level expressions", ast.len());

    let mut ctx = CompilerCtx::default();

    info!("initializing runtime");
    ctx = RuntimeBuilder::init(ctx)?;

    for expr in &ast {
        match &expr.inner {
            Expr::Machine(machine) => {
                info!("compiling machine '{}'", machine.inner.ident.to_string());
                ctx = compile_machine(ctx, &machine.inner)?;
            }
            Expr::Directive(directive) => match directive.name.as_str() {
                "rt" => {
                    let Type::Named(func_name) = &directive.args[0].inner else {
                        bail!("Except named type, but found: {:?}", directive.args[0]);
                    };
                    debug!("directive @rt: declaring '{}'", func_name.0);
                    ctx.declare_rt_func_in_prog(func_name.0);
                }
                _ => unimplemented!(),
            },
            _ => unimplemented!(),
        }
    }

    info!("building runtime entry point (_start)");
    ctx = RuntimeBuilder::build(ctx)?;

    info!("emitting object code");
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

/// Hash a branch's pattern to produce a unique u64 key and the non-default
/// parameter count.  Only the *type* of each non-default parameter is hashed
/// (not the binding name) so the hash is identical to `hash_call_args` when
/// the caller passes values of matching types.
pub(crate) fn hash_pattern(patterns: &[Pattern<'_>]) -> (u64, usize) {
    let mut param_count = 0;
    let mut hasher = DefaultHasher::new();
    for p in patterns {
        if p.default.is_none() {
            param_count += 1;
            Clean::<Type<'_>>::clean(p).to_string().hash(&mut hasher);
        }
    }
    (hasher.finish(), param_count)
}

/// Hash the types of call-site arguments to produce the same key as
/// `hash_pattern` for a branch whose parameter types match.
pub(crate) fn hash_call_args(args: &[lake_frontend::api::expr::Expr<'_>]) -> u64 {
    use lake_frontend::api::expr::Expr;
    let mut hasher = DefaultHasher::new();
    for arg in args {
        let ty_str = match arg {
            Expr::Var(_, ty) | Expr::Num(_, ty) | Expr::String(_, ty) => ty.to_string(),
            _ => continue,
        };
        ty_str.hash(&mut hasher);
    }
    hasher.finish()
}

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

    /// Compile `source`, link it, run it and return (exit_code, stdout).
    fn compile_and_run_output(source: &str) -> Result<(i32, Vec<u8>)> {
        let dir = tempdir()?;
        let src_path = dir.path().join("prog.lake");
        fs::write(&src_path, source)?;
        let bytes = compile(&src_path)?;
        link(dir.path(), "prog", &bytes)?;
        let out = Command::new(dir.path().join("prog")).output()?;
        Ok((out.status.code().unwrap_or(-1), out.stdout))
    }

    #[test]
    fn hello_world_exits_zero() -> Result<()> {
        let src = r#"@rt(rt_write) main is { n str."Hello, world!" -> { rt_write(1 n 13) } }"#;
        assert_eq!(compile_and_run(src)?, 0);
        Ok(())
    }

    #[test]
    fn string_escape_newline() -> Result<()> {
        // str."ok\n" should produce 3 bytes: 'o', 'k', '\n'
        let src = r#"@rt(rt_write) main is { n str."ok\n" -> { rt_write(1 n 3) } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok\n");
        Ok(())
    }

    #[test]
    fn num_literal_exits_zero() -> Result<()> {
        // A machine that just binds a number and does nothing else.
        let src = r#"main is { n i64.1 -> { n } }"#;
        assert_eq!(compile_and_run(src)?, 0);
        Ok(())
    }

    #[test]
    fn spawn_worker_runs() -> Result<()> {
        // worker must be declared before main (single-pass compilation).
        let src = r#"@rt(rt_write) worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n i64.0 -> { worker() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok", "worker output missing: got {:?}", stdout);
        Ok(())
    }

    #[test]
    fn spawn_two_workers_run() -> Result<()> {
        let src = r#"@rt(rt_write) worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n i64.0 -> { worker() worker() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"okok", "two workers output missing: got {:?}", stdout);
        Ok(())
    }

    #[test]
    fn spawn_worker_from_string_main() -> Result<()> {
        // main uses a string literal (like simple.lake), not a number.
        let src = r#"@rt(rt_write) worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n str."hi" -> { worker() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok", "worker output missing: got {:?}", stdout);
        Ok(())
    }

    #[test]
    fn spawn_nested_workers() -> Result<()> {
        // worker2 spawns worker3 twice (nested spawn), like simple.lake's worker2.
        let src = r#"@rt(rt_write) worker3 is { n str."w3" -> { rt_write(1 n 2) } } worker2 is { n str."w2" -> { worker3() worker3() rt_write(1 n 2) } } main is { n str."hi" -> { worker2() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert!(stdout.windows(2).any(|w| w == b"w2"), "w2 missing from {:?}", stdout);
        Ok(())
    }

    #[test]
    fn simple_lake_pattern_one_worker() -> Result<()> {
        // Minimal simple.lake-like pattern: main uses string, spawns exactly 1 worker.
        let src = r#"@rt(rt_write) worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n str."Hello, world!" -> { worker() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok", "single worker output missing: got {:?}", stdout);
        Ok(())
    }

    #[test]
    fn spawn_worker2_nested() -> Result<()> {
        // worker2 spawns 2 worker3s and calls rt_write; verify all run.
        let src = r#"@rt(rt_write) worker3 is { n str."w3" -> { rt_write(1 n 2) } } worker2 is { n str."w2" -> { worker3() worker3() rt_write(1 n 2) } } worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n str."h" -> { worker() worker2() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        // Should have: ok (from worker), w2 (from worker2), w3 w3 (from worker3×2)
        let s = std::str::from_utf8(&stdout).unwrap_or("");
        assert!(s.contains("ok"), "missing 'ok': {s:?}");
        assert!(s.contains("w2"), "missing 'w2': {s:?}");
        assert_eq!(s.matches("w3").count(), 2, "expected 2x 'w3': {s:?}");
        Ok(())
    }
}
