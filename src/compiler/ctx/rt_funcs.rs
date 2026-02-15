use anyhow::{Result, anyhow};
use cranelift::{
    codegen::ir::FuncRef,
    module::{FuncId, FuncOrDataId, Module},
    object::ObjectModule,
    prelude::FunctionBuilder,
};

/// Typed handles for every runtime function injected by the compiler.
/// Populated by `RuntimeBuilder::build()` and stored in `CompilerCtx`.
#[derive(Debug, Clone)]
pub struct RtFuncs {
    pub load_u64: FuncId,
    pub store: FuncId,
    pub write: FuncId,
    pub write_static: FuncId,
    pub exit: FuncId,
    pub mmap: FuncId,
    pub allocate: FuncId,
}

impl RtFuncs {
    /// Resolve all runtime function IDs from the module after they have been declared.
    pub fn resolve(module: &ObjectModule) -> Result<Self> {
        Ok(Self {
            load_u64: resolve_func(module, "rt_load_u64")?,
            store: resolve_func(module, "rt_store")?,
            write: resolve_func(module, "rt_write")?,
            write_static: resolve_func(module, "rt_write_static")?,
            exit: resolve_func(module, "rt_exit")?,
            mmap: resolve_func(module, "rt_mmap")?,
            allocate: resolve_func(module, "rt_allocate")?,
        })
    }

    /// Declare `load_u64` for use in the function currently being built.
    pub fn load_u64_ref(
        &self,
        module: &mut ObjectModule,
        builder: &mut FunctionBuilder,
    ) -> FuncRef {
        module.declare_func_in_func(self.load_u64, builder.func)
    }

    /// Declare `store` for use in the function currently being built.
    pub fn store_ref(&self, module: &mut ObjectModule, builder: &mut FunctionBuilder) -> FuncRef {
        module.declare_func_in_func(self.store, builder.func)
    }

    /// Declare `write` for use in the function currently being built.
    pub fn write_ref(&self, module: &mut ObjectModule, builder: &mut FunctionBuilder) -> FuncRef {
        module.declare_func_in_func(self.write, builder.func)
    }

    /// Declare `exit` for use in the function currently being built.
    pub fn exit_ref(&self, module: &mut ObjectModule, builder: &mut FunctionBuilder) -> FuncRef {
        module.declare_func_in_func(self.exit, builder.func)
    }

    pub fn allocate_ref(
        &self,
        module: &mut ObjectModule,
        builder: &mut FunctionBuilder,
    ) -> FuncRef {
        module.declare_func_in_func(self.allocate, builder.func)
    }
}

fn resolve_func(module: &ObjectModule, name: &str) -> Result<FuncId> {
    match module.get_name(name) {
        Some(FuncOrDataId::Func(id)) => Ok(id),
        Some(_) => Err(anyhow!("'{name}' is a data symbol, not a function")),
        None => Err(anyhow!("Runtime function '{name}' was not declared")),
    }
}
