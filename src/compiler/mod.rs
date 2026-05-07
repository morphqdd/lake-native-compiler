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
    ctx::{CompilerCtx, OptLevel, registry::GuardValue},
    pipeline::machine::compile_machine,
    rt::RuntimeBuilder,
};

pub mod ctx;
pub mod mphf;
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
        err.1.display(&src, path);
        anyhow!("Failed while build ast!")
    })?;
    info!("parsed {} top-level expressions", ast.1.len());
    debug!("ast: {:?}", ast.1);

    let mut ctx = CompilerCtx::new(opt);

    info!("initializing runtime");
    ctx = RuntimeBuilder::init(ctx)?;

    info!("indexing machines and patterns");
    for expr in &ast.1 {
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
    for expr in &ast.1 {
        if let Expr::Machine(machine) = &expr.inner {
            index_machine(&mut ctx, &machine.inner)?;
        }
    }

    for expr in &ast.1 {
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

/// Embedded syscall runtime object. Baked into the lakec binary at build time
/// so the compiler does not depend on CWD or external file layout.
const SYSCALL_OBJ: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/external/build/syscall.o"
));

pub fn link<BP: AsRef<Path>>(
    build_path: BP,
    name: &str,
    bytes: &[u8],
    strip: bool,
    linker: &str,
) -> Result<()> {
    fs::create_dir_all(&build_path)?;
    let obj_path = build_path.as_ref().join(format!("{name}.o"));
    let syscall_path = build_path.as_ref().join("syscall.o");
    let out_path = build_path.as_ref().join(name);
    fs::write(&obj_path, bytes)?;
    fs::write(&syscall_path, SYSCALL_OBJ)?;

    let mut args = vec![
        "-static".to_string(),
        syscall_path.to_string_lossy().into_owned(),
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
            let guards: Vec<Option<GuardValue>> = patterns
                .iter()
                .map(|p| {
                    if let Some(v) = p.guard_i64() {
                        Some(GuardValue::Int(v))
                    } else if let Some(s) = p.guard_str() {
                        Some(GuardValue::Str(s.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
            debug!(
                "index: '{name}' branch[{branch_id}] \
                 hash={hash:#018x} params={param_count} vars={var_count}"
            );
            ctx.insert_pattern(&name, hash, param_count, branch_id as u128, var_count, guards)?;
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
    let pattern_slots = branch
        .patterns
        .iter()
        .filter(|p| !p.inner.is_wildcard() && !p.inner.is_literal_guard())
        .count();
    pattern_slots + body_lets
}

/// Hash a branch's pattern to produce a unique u64 key and the non-default
/// parameter count.  Only the *type* of each non-default parameter is hashed
/// (not the binding name) so the hash is identical to `hash_call_args` when
/// the caller passes values of matching types.
pub(crate) fn hash_pattern(patterns: &[Pattern<'_>]) -> (u64, usize) {
    let mut param_count = 0;
    let mut hasher = DefaultHasher::new();
    for p in patterns {
        if p.is_wildcard() {
            continue;
        }
        param_count += 1;
        let ty = Clean::<Type<'_>>::clean(p);
        debug!("Hashed pattern ty: {ty}");
        ty.to_string().hash(&mut hasher);
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
