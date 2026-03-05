use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, anyhow, bail};
use indicatif::ProgressBar;
use lake_frontend::{
    api::{
        ast::{Branch, Clean, MachineItem, Pattern, Type},
        expr::Expr,
    },
    prelude::build_ast,
};
use log::{debug, error, info};

use crate::compiler::{
    ctx::{CompilerCtx, OptLevel},
    pipeline::machine::compile_machine,
    rt::RuntimeBuilder,
};

pub mod ctx;
pub mod pipeline;
pub mod rt;

pub fn compile<SP: AsRef<Path>>(
    pb: ProgressBar,
    source_path: SP,
    opt: OptLevel,
) -> Result<Vec<u8>> {
    let path = source_path.as_ref();
    info!("compile: {} (opt={})", path.display(), opt.as_str());

    let src = fs::read_to_string(path)?;
    let ast = build_ast(path, &src).map_err(|err| {
        pb.finish_and_clear();
        err.display(&src, path);
        anyhow!("Failed while build ast!")
    })?;
    info!("parsed {} top-level expressions", ast.len());

    let mut ctx = CompilerCtx::new(opt);

    info!("initializing runtime");
    ctx = RuntimeBuilder::init(ctx)?;

    info!("indexing machines and patterns");
    // ── Index pre-pass ───────────────────────────────────────────────────────
    // Pass 1: @rt directives + Cranelift function pre-declarations.
    for expr in &ast {
        match &expr.inner {
            Expr::Directive(directive) if directive.name.as_str() == "rt" => {
                let Type::Named(func_name) = &directive.args[0].inner else {
                    bail!("@rt expects a named type, found: {:?}", directive.args[0]);
                };
                debug!("index: @rt '{}'", func_name.0);
                ctx.declare_rt_func_in_prog(func_name.0);
            }
            Expr::Machine(machine) => {
                let name = machine.inner.ident.to_string();
                debug!("index: pre-declare machine '{name}'");
                ctx.add_machine(&name);
                ctx.predeclare_machine(&name)?;
            }
            _ => {}
        }
    }
    // Pass 2: branch patterns — compute hashes once and store in registry.
    for expr in &ast {
        if let Expr::Machine(machine) = &expr.inner {
            index_machine(&mut ctx, &machine.inner)?;
        }
    }

    for expr in &ast {
        if let Expr::Machine(machine) = &expr.inner {
            info!("compiling machine '{}'", machine.inner.ident.to_string());
            if let Err(err) = compile_machine(&mut ctx, &machine.inner, 256) {
                error!("{}", err);
                debug!("{:#?}", ctx.get_registry());
            }
        }
    }

    info!("building runtime entry point (_start)");
    ctx = RuntimeBuilder::build(ctx)?;

    info!("emitting object code");
    let obj = ctx.finish();
    Ok(obj.emit()?)
}

pub fn link<BP: AsRef<Path>>(
    build_path: BP,
    name: &str,
    bytes: &[u8],
    strip: bool,
    linker: &str,
) -> Result<()> {
    fs::create_dir_all(&build_path)?;
    let obj_path = build_path.as_ref().join(format!("{name}.o"));
    let out_path = build_path.as_ref().join(name);
    fs::write(&obj_path, bytes)?;

    let mut args = vec![
        "-static".to_string(),
        "external/build/syscall.o".to_string(),
        obj_path.to_string_lossy().into_owned(),
        "-o".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    if strip {
        args.push("--strip-all".to_string());
    }

    let ok = Command::new(linker).args(&args).status()?.success();
    if !ok {
        bail!("{linker} linker failed");
    }
    Ok(())
}

/// Index all branches of a single machine: compute pattern hashes once and
/// insert them into the registry.  Called during the pre-pass before any
/// Cranelift code generation so that forward references work.
fn index_machine(
    ctx: &mut CompilerCtx,
    machine: &lake_frontend::api::ast::Machine<'_>,
) -> Result<()> {
    let name = machine.ident.to_string();
    for (branch_id, item) in machine.items.iter().enumerate() {
        if let MachineItem::Branch(ref branch) = item.inner {
            let patterns = Clean::<Vec<Pattern<'_>>>::clean(branch);
            let (hash, param_count) = hash_pattern(&patterns);
            let var_count = count_branch_vars(branch);
            debug!(
                "index: '{name}' branch[{branch_id}] \
                 hash={hash:#018x} params={param_count} vars={var_count}"
            );
            ctx.insert_pattern(&name, hash, param_count, branch_id as u128, var_count)?;
        }
    }
    Ok(())
}

/// Count the variable slots a branch will occupy.
/// Uses `Clean<Vec<Expr>>` to get the unwrapped body expressions, then counts
/// top-level `Expr::Let` bindings.  This is the exact count for the current IR
/// where only `let` nodes and patterns allocate variable slots.
fn count_branch_vars(branch: &Branch<'_>) -> usize {
    let body: Vec<Expr<'_>> = Clean::<Vec<Expr<'_>>>::clean(branch);
    let body_lets = body
        .iter()
        .filter(|e| matches!(e, Expr::Let { .. }))
        .count();
    branch.patterns.len() + body_lets
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
            let ty = Clean::<Type<'_>>::clean(p);
            debug!("Hashed pattern ty: {ty}");
            ty.to_string().hash(&mut hasher);
        }
    }
    (hasher.finish(), param_count)
}

/// Hash the types of call-site arguments to produce the same key as
/// `hash_pattern` for a branch whose parameter types match.
///
/// `var_types` maps variable names to their Lake-level type strings as
/// declared in the enclosing branch pattern.  When the frontend emits `{}`
/// for a variable whose type is actually known (e.g. `n` declared as `i64`),
/// the map is used to recover the correct type string.
pub(crate) fn hash_call_args(
    args: &[lake_frontend::api::expr::Expr<'_>],
    var_types: &std::collections::HashMap<String, String>,
) -> u64 {
    use lake_frontend::api::expr::Expr;
    let mut hasher = DefaultHasher::new();
    for arg in args {
        debug!("Hashed arg: {:?}", arg);
        let ty_str = match arg {
            Expr::Var(name, ty) => {
                let raw = ty.to_string();
                if raw == "{}" {
                    var_types
                        .get(name.to_string().as_str())
                        .map(|s| s.as_str())
                        .unwrap_or("{}")
                        .to_string()
                } else {
                    raw
                }
            }
            Expr::Num(_, ty) | Expr::String(_, ty) => ty.to_string(),
            Expr::Jump { ident, .. } => match &ident.inner {
                Expr::Var(_, ty) => ty.to_string(),
                _ => continue,
            },
            // Arithmetic and comparison ops produce i64.
            Expr::Add(_, _)
            | Expr::Sub(_, _)
            | Expr::Mul(_, _)
            | Expr::Div(_, _)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Eq(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _) => "i64".to_string(),
            Expr::Bool(_) => "i64".to_string(),
            _ => continue,
        };
        debug!("Hashed arg ty: {ty_str}");
        ty_str.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use anyhow::Result;
    use indicatif::ProgressBar;
    use tempfile::tempdir;

    use crate::compiler::{compile, ctx::OptLevel, link};

    /// Compile `source`, link it, run it and return the exit code.
    fn compile_and_run(source: &str) -> Result<i32> {
        let dir = tempdir()?;
        let src_path = dir.path().join("prog.lake");
        fs::write(&src_path, source)?;
        let bytes = compile(ProgressBar::new(0), &src_path, OptLevel::None)?;
        link(dir.path(), "prog", &bytes, false, "mold")?;
        let status = Command::new(dir.path().join("prog")).status()?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Compile `source`, link it, run it and return (exit_code, stdout).
    fn compile_and_run_output(source: &str) -> Result<(i32, Vec<u8>)> {
        let dir = tempdir()?;
        let src_path = dir.path().join("prog.lake");
        fs::write(&src_path, source)?;
        let bytes = compile(ProgressBar::new(0), &src_path, OptLevel::None)?;
        link(dir.path(), "prog", &bytes, false, "mold")?;
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
        assert_eq!(
            stdout, b"okok",
            "two workers output missing: got {:?}",
            stdout
        );
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
        assert!(
            stdout.windows(2).any(|w| w == b"w2"),
            "w2 missing from {:?}",
            stdout
        );
        Ok(())
    }

    #[test]
    fn simple_lake_pattern_one_worker() -> Result<()> {
        // Minimal simple.lake-like pattern: main uses string, spawns exactly 1 worker.
        let src = r#"@rt(rt_write) worker is { n str."ok" -> { rt_write(1 n 2) } } main is { n str."Hello, world!" -> { worker() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(
            stdout, b"ok",
            "single worker output missing: got {:?}",
            stdout
        );
        Ok(())
    }

    #[test]
    fn state_transition_self() -> Result<()> {
        // `fsm` has two branches distinguished by argument type:
        //   branch 0 — 0 non-default params (default str."A"), calls self(str."B")
        //              to transition to branch 1.
        //   branch 1 — 1 required str param, writes it to stdout.
        // Expected output: "B" (branch 0 transitions, branch 1 executes).
        let src = r#"@rt(rt_write) fsm is { _ str."A" -> { self("B") } n str -> { rt_write(1 n 1) } } main is { _ i64.0 -> { fsm() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(
            stdout, b"B",
            "state transition output missing: got {:?}",
            stdout
        );
        Ok(())
    }

    #[test]
    fn when_false_branch_runs() -> Result<()> {
        let src = r#"@rt(rt_write) main is { _ i64.0 -> { when false { false -> { rt_write(1 "no" 2) } true -> { rt_write(1 "yes" 3) } } } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"no");
        Ok(())
    }

    #[test]
    fn when_true_branch_runs() -> Result<()> {
        let src = r#"@rt(rt_write) main is { _ i64.0 -> { when true { false -> { rt_write(1 "no" 2) } true -> { rt_write(1 "yes" 3) } } } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"yes");
        Ok(())
    }

    #[test]
    fn when_no_match_continues() -> Result<()> {
        // No branch matches → silent fallthrough, no output.
        let src = r#"@rt(rt_write) main is { _ i64.0 -> { when 42 { 0 -> { rt_write(1 "zero" 4) } } } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"");
        Ok(())
    }

    #[test]
    fn forward_ref_worker_declared_after_main() -> Result<()> {
        // main is declared BEFORE worker — this requires the index pre-pass
        // to predeclare worker's Cranelift function before main is compiled.
        let src = r#"@rt(rt_write) main is { n i64.0 -> { worker() } } worker is { n str."ok" -> { rt_write(1 n 2) } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(
            stdout, b"ok",
            "forward-ref worker output missing: got {:?}",
            stdout
        );
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

    #[test]
    fn self_loop_terminates() -> Result<()> {
        // counter(3): counts down via self(), writes "done" when n == 0.
        let src = r#"@rt(rt_write) counter is { n i64 -> { when 0 == n { true -> { rt_write(1 "done" 4) } false -> { self(n-1) } } } } main is { _ i64.0 -> { counter(3) } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"done");
        Ok(())
    }

    #[test]
    fn when_numeric_three_branches() -> Result<()> {
        // when with 3 numeric branches — Cranelift Switch dispatches to branch 2.
        let src = r#"@rt(rt_write) main is { _ i64.0 -> { when 2 { 0 -> { rt_write(1 "zero" 4) } 1 -> { rt_write(1 "one" 3) } 2 -> { rt_write(1 "two" 3) } } } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"two");
        Ok(())
    }

    #[test]
    fn arithmetic_accumulator_in_self_args() -> Result<()> {
        // adder(3 0): accumulates 3+2+1=6, checks result with nested when.
        let src = r#"@rt(rt_write) adder is { n i64 acc i64 -> { when 0 == n { true -> { when acc == 6 { true -> { rt_write(1 "ok" 2) } false -> { rt_write(1 "fail" 4) } } } false -> { self(n-1 acc+n) } } } } main is { _ i64.0 -> { adder(3 0) } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok");
        Ok(())
    }

    #[test]
    fn when_after_state_transition() -> Result<()> {
        // machine transitions to second branch via self(42), then uses when.
        let src = r#"@rt(rt_write) m is { _ i64.0 -> { self(42) } n i64 -> { when n == 42 { true -> { rt_write(1 "ok" 2) } false -> { rt_write(1 "no" 2) } } } } main is { _ i64.0 -> { m() } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"ok");
        Ok(())
    }

    #[test]
    fn two_concurrent_self_loops() -> Result<()> {
        // Two cnt workers run concurrently, each counts down and writes "x" once.
        let src = r#"@rt(rt_write) cnt is { n i64 -> { when 0 == n { true -> { rt_write(1 "x" 1) } false -> { self(n-1) } } } } main is { _ i64.0 -> { cnt(2) cnt(2) } }"#;
        let (code, stdout) = compile_and_run_output(src)?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"xx");
        Ok(())
    }
}
