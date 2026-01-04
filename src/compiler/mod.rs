use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    process::Command,
};

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::BlockArg,
    frontend::Switch,
    module::{Linkage, Module},
    prelude::{AbiParam, FunctionBuilder, FunctionBuilderContext, InstBuilder},
};
use lake_frontend::{
    api::ast::{Machine, Pattern},
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

fn hash(patterns: &[Pattern<'_>]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for pattern in patterns {
        if !pattern.has_default() {
            let ident = pattern.ident();
            let ty = pattern.ty();
            ident.hash(&mut hasher);
            ty.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn compile_machine(mut ctx: CompilerCtx, machine: &Machine<'_>) -> Result<CompilerCtx> {
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

    for (i, branch) in machine.branches().iter().enumerate() {
        let branch_block = builder.create_block();
        builder.switch_to_block(branch_block);
        switch.set_entry(i as u128, branch_block);
        builder.ins().return_(&[]);
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
