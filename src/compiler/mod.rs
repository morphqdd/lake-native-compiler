use std::{
    collections::HashMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, bail};
use base64ct::{Base64, Encoding};
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
        ast::{Branch, Ident, Machine, Pattern, Type},
        expr::Expr,
    },
    prelude::parse,
};

use crate::compiler::{ctx::CompilerCtx, rt::Runtime};

mod ctx;
mod rt;

pub const CTX_SIZE: i64 = 40;
pub const BRANCH_ID: i64 = 0;
pub const BLOCK_ID: i64 = 8;
pub const TEMP_VAL: i64 = 16;
pub const VARIABLES: i64 = 24;
pub const JUMP_ARGS: i64 = 32;

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

fn hash_pattern(patterns: &[Pattern<'_>]) -> (u64, usize) {
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
    builder.func.signature.returns.push(AbiParam::new(ptr_ty));

    let entry = builder.create_block();
    let default_block = builder.create_block();
    let switch_block = builder.create_block();
    builder.append_block_param(entry, ptr_ty);

    builder.switch_to_block(entry);
    let ctx_ptr = builder.block_params(entry)[0];
    let machine_ctx_var = builder.declare_var(ptr_ty);
    builder.def_var(machine_ctx_var, ctx_ptr);

    builder.ins().jump(switch_block, &[]);

    let mut switch = Switch::new();

    for (block_id, branch) in machine.branches().iter().enumerate() {
        ctx = compile_branch(
            ctx,
            &mut builder,
            &machine_ident,
            &mut switch,
            block_id as u128,
            branch,
            machine_ctx_var,
        )?;
    }

    builder.switch_to_block(switch_block);
    let ctx_ptr = builder.use_var(machine_ctx_var);

    let Some(FuncOrDataId::Func(rt_load_u64_id)) = ctx.module().get_name("rt_load_u64") else {
        bail!("rt_load_u64 is not declare");
    };

    let rt_load_u64_ref = ctx
        .module_mut()
        .declare_func_in_func(rt_load_u64_id, &mut builder.func);

    let branch_id_offset = builder.ins().iconst(ptr_ty, BRANCH_ID);
    let call_inst = builder
        .ins()
        .call(rt_load_u64_ref, &[ctx_ptr, branch_id_offset]);
    let branch_id = builder.inst_results(call_inst)[0];
    switch.emit(&mut builder, branch_id, default_block);

    builder.switch_to_block(default_block);

    let neg = builder.ins().iconst(ptr_ty, -1);
    builder.ins().return_(&[neg]);

    builder.seal_all_blocks();

    let machine_sig = builder.func.signature.clone();
    let machine_ident = machine.ident().to_string();
    let id = ctx
        .module_mut()
        .declare_function(&machine_ident, Linkage::Export, &machine_sig)?;

    println!("{machine_ident}: {}", module_ctx.func);
    ctx.module_mut().define_function(id, &mut module_ctx)?;

    ctx.module_mut().clear_context(&mut module_ctx);

    Ok(ctx)
}

fn compile_branch(
    mut ctx: CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ident: &str,
    switch: &mut Switch,
    branch_id: u128,
    branch: &Branch<'_>,
    machine_ctx_var: Variable,
) -> Result<CompilerCtx> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let patterns = branch.patterns();
    let (hash, count) = hash_pattern(&patterns);

    let branch_entry_block = builder.create_block();
    let branch_switch_block = builder.create_block();
    builder.append_block_param(branch_switch_block, ptr_ty);
    let default_branch_block = builder.create_block();
    builder.switch_to_block(default_branch_block);
    let neg = builder.ins().iconst(ptr_ty, -1);
    builder.ins().return_(&[neg]);

    switch.set_entry(branch_id, branch_entry_block);

    builder.switch_to_block(branch_entry_block);

    let ctx_ptr = builder.use_var(machine_ctx_var);
    let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
    let block_id_offset = builder.ins().iconst(ptr_ty, BLOCK_ID);

    let call = builder
        .ins()
        .call(load_u64_ref, &[ctx_ptr, block_id_offset]);

    let block_id = builder.inst_results(call)[0];

    builder
        .ins()
        .jump(branch_switch_block, &[BlockArg::Value(block_id)]);

    let mut state = HashMap::new();

    // for pattern in patterns {
    //     if pattern.has_default() {
    //         let ident = pattern.ident();
    //         let ty_str = pattern.ty().to_string();
    //         let ty = ctx.lookup_type(&ty_str).unwrap().clone();
    //         let var_id = compile_expr(&mut ctx, builder, &state, &pattern.default().unwrap())?;
    //         state.insert(ident.to_string(), var_id);
    //     }
    // }

    let mut branch_switch = Switch::new();
    let mut block_id = 0;
    for pattern in &patterns {
        if pattern.has_default() {
            let next_block = compile_expr(
                &mut ctx,
                builder,
                machine_ctx_var,
                block_id,
                &mut branch_switch,
                &mut state,
                &Expr::from(pattern),
            )?;
            block_id = next_block
        }
    }

    let body = branch.body();

    for expr in &body {
        let next_block = compile_expr(
            &mut ctx,
            builder,
            machine_ctx_var,
            block_id,
            &mut branch_switch,
            &mut state,
            expr,
        )?;
        block_id = next_block
    }

    builder.switch_to_block(branch_switch_block);
    let block_id = builder.block_params(branch_switch_block)[0];
    branch_switch.emit(builder, block_id, default_branch_block);

    println!("{hash} state: {state:?}");

    ctx.insert_pattern(
        &machine_ident,
        hash,
        count,
        branch_id,
        state.values().count(),
    )?;
    Ok(ctx)
}

fn compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut HashMap<String, (cranelift::prelude::Type, usize)>,
    expr: &Expr<'_>,
) -> Result<i64> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    println!("BLOCK_ID: {block_id}");
    match expr {
        Expr::Let { ident, ty, default } => {
            let next_id = match default {
                Some(default) => compile_expr(
                    ctx,
                    builder,
                    machine_ctx_var,
                    block_id,
                    branch_switch,
                    state,
                    default,
                )?,
                None => block_id,
            };

            let b = builder.create_block();
            builder.switch_to_block(b);

            let var_index = state
                .values()
                .map(|(_, x)| x)
                .max()
                .map(|x| x + 1)
                .unwrap_or(0);

            state.insert(
                ident.to_string(),
                (
                    ctx.lookup_type(&ty.to_string())
                        .cloned()
                        .unwrap()
                        .unwrap_simple(),
                    var_index,
                ),
            );

            let ctx_ptr = builder.use_var(machine_ctx_var);
            let store_ref = ctx.get_func(builder, "rt_store")?;
            let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
            let temp_val_offset = builder.ins().iconst(ptr_ty, TEMP_VAL);
            let call = builder
                .ins()
                .call(load_u64_ref, &[ctx_ptr, temp_val_offset]);
            let temp_val = builder.inst_results(call)[0];
            let size = builder.ins().iconst(ptr_ty, 8);
            let var_offset = builder.ins().iconst(ptr_ty, VARIABLES);

            let call = builder.ins().call(load_u64_ref, &[ctx_ptr, var_offset]);
            let var_ptr = builder.inst_results(call)[0];
            let var_offset = builder.ins().iconst(ptr_ty, var_index as i64 * 8);
            builder
                .ins()
                .call(store_ref, &[var_ptr, temp_val, size, var_offset]);
            let next_id_val = builder.ins().iconst(ptr_ty, next_id as i64 + 1);
            builder.ins().return_(&[next_id_val]);

            branch_switch.set_entry(next_id as u128, b);

            Ok(next_id + 1)
        }
        Expr::String(str) => {
            let b = builder.create_block();
            builder.switch_to_block(b);

            let mut hasher = DefaultHasher::new();
            str.hash(&mut hasher);
            let hash = hasher.finish();
            let encoded_str = Base64::encode_string(&hash.to_be_bytes());
            let data_id = match ctx.module().get_name(&encoded_str) {
                Some(FuncOrDataId::Data(str_id)) => str_id,
                _ => ctx
                    .module_mut()
                    .declare_data(&encoded_str, Linkage::Export, false, false)?,
            };
            let mut data = DataDescription::new();
            data.define(str.as_bytes().to_vec().into_boxed_slice());
            ctx.module_mut().define_data(data_id, &data)?;

            let data_gv = ctx
                .module_mut()
                .declare_data_in_func(data_id, &mut builder.func);
            let data_ptr = builder.ins().global_value(ptr_ty, data_gv);

            let data_fat_ptr_name = format!("fp_{encoded_str}");
            let data_fat_ptr_id = match ctx.module().get_name(&data_fat_ptr_name) {
                Some(FuncOrDataId::Data(id)) => id,
                _ => ctx.module_mut().declare_data(
                    &data_fat_ptr_name,
                    Linkage::Export,
                    true,
                    false,
                )?,
            };

            let mut fat_ptr_data = DataDescription::new();
            fat_ptr_data.define_zeroinit(16);
            ctx.module_mut()
                .define_data(data_fat_ptr_id, &fat_ptr_data)?;

            let data_fat_ptr_gv = ctx
                .module_mut()
                .declare_data_in_func(data_fat_ptr_id, builder.func);

            let data_fat_ptr = builder.ins().global_value(ptr_ty, data_fat_ptr_gv);
            let end_ptr = builder
                .ins()
                .iadd_imm(data_ptr, str.as_bytes().len() as i64);

            builder
                .ins()
                .store(MemFlags::new(), data_ptr, data_fat_ptr, 0);
            builder
                .ins()
                .store(MemFlags::new(), end_ptr, data_fat_ptr, 8);

            let ctx_ptr = builder.use_var(machine_ctx_var);
            let store_ref = ctx.get_func(builder, "rt_store")?;
            let temp_val_offset = builder.ins().iconst(ptr_ty, TEMP_VAL);
            let size = builder.ins().iconst(ptr_ty, 8);

            builder
                .ins()
                .call(store_ref, &[ctx_ptr, data_fat_ptr, size, temp_val_offset]);

            let next_id_val = builder.ins().iconst(ptr_ty, block_id as i64 + 1);
            builder.ins().return_(&[next_id_val]);

            branch_switch.set_entry(block_id as u128, b);

            Ok(block_id + 1)
        }
        Expr::Jump { ident, args } => {
            let mut next_id = block_id;

            for (i, arg) in args.iter().enumerate() {
                let id = compile_expr(
                    ctx,
                    builder,
                    machine_ctx_var,
                    next_id,
                    branch_switch,
                    state,
                    &arg,
                )?;

                let b = builder.create_block();
                builder.switch_to_block(b);

                let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
                let ctx_ptr = builder.use_var(machine_ctx_var);
                let temp_val_offset = builder.ins().iconst(ptr_ty, TEMP_VAL);
                let load_call = builder
                    .ins()
                    .call(load_u64_ref, &[ctx_ptr, temp_val_offset]);
                let arg_val = builder.inst_results(load_call)[0];

                let jump_args_offset = builder.ins().iconst(ptr_ty, JUMP_ARGS);
                let load_call = builder
                    .ins()
                    .call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
                let args_ptr = builder.inst_results(load_call)[0];
                let store_ref = ctx.get_func(builder, "rt_store")?;
                let size = builder.ins().iconst(ptr_ty, 8);
                let offset = builder.ins().iconst(ptr_ty, i as i64 * 8);
                builder
                    .ins()
                    .call(store_ref, &[args_ptr, arg_val, size, offset]);
                let next_block = builder.ins().iconst(ptr_ty, id + 1);
                builder.ins().return_(&[next_block]);

                branch_switch.set_entry(id as u128, b);

                next_id = id + 1;
            }

            let b = builder.create_block();
            builder.switch_to_block(b);

            let Expr::Var(ident) = ident.inner else {
                bail!("Ident must be var")
            };

            let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
            let ctx_ptr = builder.use_var(machine_ctx_var);
            let jump_args_offset = builder.ins().iconst(ptr_ty, JUMP_ARGS);
            let load_call = builder
                .ins()
                .call(load_u64_ref, &[ctx_ptr, jump_args_offset]);
            let args_ptr = builder.inst_results(load_call)[0];

            let mut vals = vec![];

            for i in 0..args.len() {
                let args_offset = builder.ins().iconst(ptr_ty, i as i64 * 8);
                let call = builder.ins().call(load_u64_ref, &[args_ptr, args_offset]);
                let val = builder.inst_results(call)[0];
                vals.push(val);
            }

            let func_ref = ctx.get_func(builder, ident)?;

            builder.ins().call(func_ref, &vals);

            let next_block = builder.ins().iconst(ptr_ty, -1);
            builder.ins().return_(&[next_block]);

            branch_switch.set_entry(next_id as u128, b);
            Ok(next_id + 1)
        }
        Expr::Num(num_str) => {
            let num = num_str.parse::<i64>()?;

            let b = builder.create_block();
            builder.switch_to_block(b);

            let num_val = builder.ins().iconst(ptr_ty, num);

            let ctx_ptr = builder.use_var(machine_ctx_var);
            let store_ref = ctx.get_func(builder, "rt_store")?;
            let temp_val_offset = builder.ins().iconst(ptr_ty, TEMP_VAL);
            let size = builder.ins().iconst(ptr_ty, 8);

            builder
                .ins()
                .call(store_ref, &[ctx_ptr, num_val, size, temp_val_offset]);

            let next_id_val = builder.ins().iconst(ptr_ty, block_id + 1);
            builder.ins().return_(&[next_id_val]);
            branch_switch.set_entry(block_id as u128, b);
            Ok(block_id + 1)
        }
        Expr::Var(v) => {
            let (_, var_index) = state.get(&v.to_string()).unwrap();

            let b = builder.create_block();
            builder.switch_to_block(b);

            let var_val_offset = builder.ins().iconst(ptr_ty, *var_index as i64 * 8);
            let load_u64_ref = ctx.get_func(builder, "rt_load_u64")?;
            let vars_offset = builder.ins().iconst(ptr_ty, VARIABLES);
            let ctx_ptr = builder.use_var(machine_ctx_var);
            let call = builder.ins().call(load_u64_ref, &[ctx_ptr, vars_offset]);
            let vars_ptr = builder.inst_results(call)[0];

            let call = builder
                .ins()
                .call(load_u64_ref, &[vars_ptr, var_val_offset]);
            let val = builder.inst_results(call)[0];

            let ctx_ptr = builder.use_var(machine_ctx_var);
            let store_ref = ctx.get_func(builder, "rt_store")?;
            let temp_val_offset = builder.ins().iconst(ptr_ty, TEMP_VAL);
            let size = builder.ins().iconst(ptr_ty, 8);

            builder
                .ins()
                .call(store_ref, &[ctx_ptr, val, size, temp_val_offset]);

            let next_id_val = builder.ins().iconst(ptr_ty, block_id + 1);
            builder.ins().return_(&[next_id_val]);

            branch_switch.set_entry(block_id as u128, b);

            Ok(block_id + 1)
        }
        _ => todo!(),
    }
}

fn _compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ctx_ptr: Variable,
    state: &HashMap<String, usize>,
    expr: &Expr<'_>,
) -> Result<usize> {
    let ptr_ty = ctx.module().target_config().pointer_type();
    let d: Result<Value> = match expr {
        Expr::Num(num_str) => Ok(builder.ins().iconst(
            cranelift::prelude::Type::int(64).unwrap(),
            num_str.parse::<i64>()?,
        )),
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
        // Expr::Var(v) => Ok(builder.use_var(state.get(*v).unwrap().clone())),
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
        _ => todo!(),
    };

    todo!()
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
