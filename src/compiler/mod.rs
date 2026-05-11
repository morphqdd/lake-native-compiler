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
        ast::{Branch, Clean, Item, MachineItem, Pattern, Type},
        expr::Expr,
    },
    prelude::{build_program, load_and_build},
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

    // ── Phase 1: load all sources (entry + transitively imported) ──────────
    let sources = load_and_build(path).map_err(|errs| {
        pb.finish_and_clear();
        // Best-effort diagnostic display: each error has a span into one
        // of the loaded files; we render against the entry file as a
        // fallback when the loader fails before parsing succeeds.
        let entry_src = fs::read_to_string(path).unwrap_or_default();
        errs.display(&entry_src, path);
        anyhow!("Failed to load Lake program")
    })?;

    // ── Phase 2: parse → populate registry → resolve → typecheck ───────────
    let lake_program = build_program(&sources).map_err(|errs| {
        pb.finish_and_clear();
        // Render diagnostics against every loaded file in turn; spans
        // outside the file's range simply don't render.
        for f in sources.files() {
            errs.display(&f.src, &f.source_path);
        }
        anyhow!("Failed while build ast!")
    })?;

    let module_count = lake_program.program.modules.len();
    let item_count: usize = lake_program
        .program
        .modules
        .iter()
        .map(|m| m.ast.len())
        .sum();
    info!(
        "parsed {} module{} ({} top-level items)",
        module_count,
        if module_count == 1 { "" } else { "s" },
        item_count
    );

    let mut ctx = CompilerCtx::new(opt);

    info!("initializing runtime");
    ctx = RuntimeBuilder::init(ctx)?;

    // Iterate every loaded module's items in turn for each pass.
    // Cross-module name collisions (two `pub` machines with the same
    // name in different modules) would surface through
    // `predeclare_machine`'s registry — for now we assume globally
    // unique machine names.  Real mangling lands when the first
    // collision in stdlib forces it.

    info!("indexing machines and patterns");
    for module in &lake_program.program.modules {
        for item in &module.ast {
            match &item.inner {
                Item::Directive(directive) if directive.name.as_str() == "rt" => {
                    let Type::Named(func_name) = &directive.args[0].inner else {
                        bail!("@rt expects a named type, found: {:?}", directive.args[0]);
                    };
                    debug!("index: @rt '{}'", func_name.0);
                    ctx.declare_rt_func_in_prog(func_name.0);
                }
                Item::Machine(machine) => {
                    let name = machine.inner.ident.to_string();
                    debug!("index: pre-declare machine '{name}'");
                    ctx.add_machine(&name);
                    ctx.predeclare_machine(&name)?;
                }
                _ => {}
            }
        }
    }
    // Pass 2: branch patterns — compute hashes once and store in registry.
    for module in &lake_program.program.modules {
        for item in &module.ast {
            if let Item::Machine(machine) = &item.inner {
                index_machine(&mut ctx, &machine.inner)?;
            }
        }
    }

    for module in &lake_program.program.modules {
        for item in &module.ast {
            if let Item::Machine(machine) = &item.inner {
                info!("compiling machine '{}'", machine.inner.ident.to_string());
                let quantum: i64 = std::env::var("LAKE_QUANTUM")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(256);
                if let Err(err) = compile_machine(&mut ctx, &machine.inner, quantum) {
                    error!("{}", err);
                    debug!("{:#?}", ctx.get_registry());
                }
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
///
/// Slots come from three places:
///
///   * The branch's own pattern positions — every position takes one slot
///     so the spawner / change_state writes line up with arg indices,
///     including wildcards and literal guards (see branch.rs).
///
///   * `let` bindings anywhere in the body — top-level, inside `when`
///     arms, and inside `wait` handler bodies.  Lowering of ret-machines
///     introduces nested lets in the form of `let __ret_N_pid = M(self
///     args); wait __ret_N_pid { ... let __ret_M_pid = ... }`, so we
///     have to recurse rather than count top-level lets only.
///
///   * `wait` handler pattern positions — each handler's
///     non-wildcard, non-guard parameter consumes a slot at codegen
///     time (`wait_expr.rs::compile`).  Sibling handlers share slots
///     so we take the maximum across them rather than the sum.
fn count_branch_vars(branch: &Branch<'_>) -> usize {
    let body_slots: usize = branch
        .body
        .iter()
        .map(|e| count_expr_slots(&e.inner))
        .sum();
    branch.patterns.len() + body_slots
}

fn count_expr_slots(expr: &Expr<'_>) -> usize {
    match expr {
        Expr::Let { default, .. } => {
            // The let itself contributes one slot, plus whatever its
            // initializer expression introduces.
            1 + default
                .as_ref()
                .map(|d| count_expr_slots(&d.inner))
                .unwrap_or(0)
        }
        Expr::When { branches, cond } => {
            count_expr_slots(&cond.inner)
                + branches
                    .iter()
                    .map(|(_, body)| body.iter().map(|e| count_expr_slots(&e.inner)).sum::<usize>())
                    .max()
                    .unwrap_or(0)
        }
        Expr::Wait { handlers, filter } => {
            let filter_slots: usize =
                filter.iter().map(|f| count_expr_slots(&f.inner)).sum();
            // All wait handlers share the same slot range — see
            // `wait_expr.rs`.  Take the maximum across siblings rather
            // than the sum.
            let handler_max = handlers
                .iter()
                .map(|h| {
                    let pat_slots = h
                        .inner
                        .patterns
                        .iter()
                        .filter(|p| !p.inner.is_wildcard() && !p.inner.is_literal_guard())
                        .count();
                    let body_slots: usize = h
                        .inner
                        .body
                        .iter()
                        .map(|e| count_expr_slots(&e.inner))
                        .sum();
                    pat_slots + body_slots
                })
                .max()
                .unwrap_or(0);
            filter_slots + handler_max
        }
        Expr::Jump { ident, args } => {
            count_expr_slots(&ident.inner)
                + args.iter().map(|a| count_expr_slots(&a.inner)).sum::<usize>()
        }
        Expr::MethodCall { receiver, args, .. } => {
            count_expr_slots(&receiver.inner)
                + args.iter().map(|a| count_expr_slots(&a.inner)).sum::<usize>()
        }
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::Ge(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => count_expr_slots(&l.inner) + count_expr_slots(&r.inner),
        Expr::Neg(inner) | Expr::Ret(inner) => count_expr_slots(&inner.inner),
        _ => 0,
    }
}

/// Collapse Lake-surface type names that share the runtime ABI to a
/// single canonical form.  At the symbol layer `str`, `atom`, `pid`,
/// and a generic i64 buffer are all 64-bit values, so the dispatch
/// hash treats them as one type.  Without this, an `at(buf i64)`
/// branch would refuse a call site that passes a `str` literal even
/// though the runtime cannot tell them apart.
fn canon_arg_ty(s: &str) -> &str {
    match s {
        "str" | "atom" | "pid" | "buf" => "i64",
        other => other,
    }
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
        let ty_str = ty.to_string();
        let canon = canon_arg_ty(&ty_str);
        debug!("Hashed pattern ty: {ty} → {canon}");
        canon.hash(&mut hasher);
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
                // The resolver leaves `Type::Unknown` (rendered as `?`) for
                // variable references whose type is determined later by the
                // enclosing pattern.  Recover the declared type from
                // `var_types` in that case.
                if raw == "?" {
                    var_types
                        .get(name.to_string().as_str())
                        .map(|s| s.as_str())
                        .unwrap_or("?")
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
            // Arithmetic, negation, and comparison ops produce i64.
            Expr::Add(_, _)
            | Expr::Sub(_, _)
            | Expr::Mul(_, _)
            | Expr::Div(_, _)
            | Expr::Neg(_)
            | Expr::Le(_, _)
            | Expr::Ge(_, _)
            | Expr::Eq(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _) => "i64".to_string(),
            Expr::Bool(_) => "i64".to_string(),
            _ => continue,
        };
        let canon = canon_arg_ty(&ty_str);
        debug!("Hashed arg ty: {ty_str} → {canon}");
        canon.hash(&mut hasher);
    }
    hasher.finish()
}
