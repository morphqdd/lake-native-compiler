use std::{
    collections::HashMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{DataDescription, FuncOrDataId, Linkage, Module},
    prelude::{
        AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder, MemFlags, Value, Variable,
    },
};
use lake_frontend::{
    api::{
        ast::{Branch, Machine, Pattern, Type},
        expr::Expr,
    },
    prelude::parse,
};

use crate::compiler::{ctx::CompilerCtx, rt::Runtime};

mod ctx;
mod rt;

pub fn link<BP: AsRef<Path>>(build_path: BP, name: &str, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(&build_path)?;
    fs::write(&build_path.as_ref().join(format!("{name}.o")), bytes)?;

    assert!(
        Command::new("mold")
            .args([
                "-static",
                "external/build/syscall.o",
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

    let rt = Runtime::default();
    ctx = rt.init(ctx)?;

    for machine in &ast {
        match compile_machine(ctx, machine) {
            Ok(changed_ctx) => ctx = changed_ctx,
            Err(err) => bail!(err),
        }
    }

    println!("machines: {:?}", ctx.machines());
    ctx = rt.build(ctx)?;

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

fn hash(patterns: &[Pattern<'_>]) -> (u64, usize) {
    let mut param_count = 0;
    let mut hasher = DefaultHasher::new();
    for pattern in patterns {
        if !pattern.has_default() {
            param_count += 1;
            let ident = pattern.ident();
            let ty = pattern.ty();
            ident.hash(&mut hasher);
            ty.to_string().hash(&mut hasher);
        }
    }
    (hasher.finish(), param_count)
}

fn compile_machine(mut ctx: CompilerCtx, machine: &Machine<'_>) -> Result<CompilerCtx> {
    let machine_ident = machine.ident().to_string();
    ctx.add_machine(&machine_ident);

    let ptr_ty = ctx.module().target_config().pointer_type();
    let mut module_ctx = ctx.module().make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut module_ctx.func, &mut builder_ctx);

    builder.func.signature.params.push(AbiParam::new(ptr_ty));

    let entry = builder.create_block();
    let default_block = builder.create_block();
    let switch_block = builder.create_block();
    builder.append_block_param(entry, ptr_ty);
    builder.append_block_param(switch_block, ptr_ty);

    builder.switch_to_block(entry);
    let val = builder.block_params(entry)[0];
    builder.ins().jump(switch_block, &[BlockArg::Value(val)]);

    let mut switch = Switch::new();

    for (block_id, branch) in machine.branches().iter().enumerate() {
        ctx = compile_branch(
            ctx,
            &mut builder,
            &machine_ident,
            &mut switch,
            block_id as u128,
            branch,
        )?;
    }

    builder.switch_to_block(switch_block);
    let val = builder.block_params(switch_block)[0];
    switch.emit(&mut builder, val, default_block);

    builder.switch_to_block(default_block);
    builder.ins().return_(&[]);

    builder.seal_all_blocks();

    let machine_sig = builder.func.signature.clone();
    let machine_ident = machine.ident().to_string();
    let id = ctx
        .module_mut()
        .declare_function(&machine_ident, Linkage::Export, &machine_sig)?;
    ctx.module_mut().define_function(id, &mut module_ctx)?;

    println!("{machine_ident}: {}", module_ctx.func);

    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

fn compile_branch(
    mut ctx: CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ident: &str,
    switch: &mut Switch,
    block_id: u128,
    branch: &Branch<'_>,
) -> Result<CompilerCtx> {
    let patterns = branch.patterns();
    let (hash, count) = hash(&patterns);
    ctx.insert_pattern(&machine_ident, hash, count, block_id)?;

    let branch_block = builder.create_block();
    builder.switch_to_block(branch_block);
    switch.set_entry(block_id, branch_block);

    let mut state = HashMap::new();

    for pattern in patterns {
        if pattern.has_default() {
            let ident = pattern.ident();
            let ty_str = pattern.ty().to_string();
            let ty = ctx.lookup_type(&ty_str).unwrap().clone();
            let default_val = compile_expr(
                &mut ctx,
                builder,
                &state,
                pattern.default().unwrap(),
                &ty_str,
            )?;
            let var = builder.declare_var(ty.unwrap_simple());
            builder.def_var(var, default_val);
            state.insert(ident.to_string(), var);
        }
    }

    let body = branch.body();

    for expr in body {
        match expr {
            Expr::Jump { ident, args } => match ident.inner {
                Expr::Var(v) => match ctx.module().get_name(v) {
                    Some(FuncOrDataId::Func(func_id)) => {
                        let mut parsed_args = vec![];
                        for arg in args {
                            let val = compile_expr(&mut ctx, builder, &state, arg.inner, "i64")?;
                            parsed_args.push(val);
                        }
                        let func_ref = ctx
                            .module_mut()
                            .declare_func_in_func(func_id, &mut builder.func);
                        builder.ins().call(func_ref, &parsed_args);
                    }
                    _ => bail!("Machine is not declare: {v}"),
                },
                _ => todo!(),
            },
            _ => todo!(),
        }
    }

    builder.ins().return_(&[]);

    println!("{hash} state: {state:?}");

    Ok(ctx)
}

fn compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    state: &HashMap<String, Variable>,
    expr: Expr<'_>,
    ty: &str,
) -> Result<Value> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    match expr {
        Expr::Num(num_str) => match ty {
            "i64" => Ok(builder.ins().iconst(
                cranelift::prelude::Type::int(64).unwrap(),
                num_str.parse::<i64>()?,
            )),
            _ => todo!(),
        },
        Expr::String(str) => {
            let data_id = match ctx.module().get_name(str) {
                Some(FuncOrDataId::Data(str_id)) => str_id,
                _ => ctx
                    .module_mut()
                    .declare_data(str, Linkage::Export, false, false)?,
            };
            let mut data = DataDescription::new();
            data.define(str.as_bytes().to_vec().into_boxed_slice());
            ctx.module_mut().define_data(data_id, &data)?;

            let data_gv = ctx
                .module_mut()
                .declare_data_in_func(data_id, &mut builder.func);
            let data_ptr = builder.ins().global_value(ptr_ty, data_gv);
            Ok(data_ptr)
        }
        Expr::Var(v) => Ok(builder.use_var(state.get(v).unwrap().clone())),
        Expr::Bool(_) => todo!(),
        Expr::Path(path) => todo!(),
        Expr::Let { ident, ty, default } => todo!(),
        Expr::Jump { ident, args } => todo!(),
        Expr::Mul(spanned, spanned1) => todo!(),
        Expr::Div(spanned, spanned1) => todo!(),
        Expr::Add(spanned, spanned1) => todo!(),
        Expr::Sub(spanned, spanned1) => todo!(),
        Expr::Branch { var_pattern, body } => todo!(),
        Expr::Machine { ident, branches } => todo!(),
    }
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
