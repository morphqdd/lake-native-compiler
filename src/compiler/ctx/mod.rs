use std::collections::HashMap;

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::FuncRef,
    module::{FuncId, FuncOrDataId, Module, default_libcall_names},
    native,
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{Configurable, FunctionBuilder, Type, settings},
};

pub mod compiler_type;
pub mod registry;
pub mod rt_funcs;

use compiler_type::CompilerType;
use registry::MachineRegistry;
use rt_funcs::RtFuncs;

pub struct CompilerCtx {
    module: ObjectModule,
    /// Registry of all machines and their branch metadata.
    registry: MachineRegistry,
    /// Type map: Lake type name → Cranelift type.
    ty_map: HashMap<String, CompilerType>,
    /// Typed handles to runtime functions. Set after `RuntimeBuilder::build()`.
    rt_funcs: Option<RtFuncs>,
    /// Cache of FuncRef declarations per (current_function_name, callee_name).
    func_ref_cache: HashMap<(String, String), FuncRef>,
}

impl Default for CompilerCtx {
    fn default() -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "speed_and_size").unwrap();
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder.set("is_pic", "false").unwrap();
        let isa_builder =
            native::builder().unwrap_or_else(|msg| panic!("Host machine is not supported: {msg}"));
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .unwrap();
        let builder = ObjectBuilder::new(isa, "lake-program", default_libcall_names()).unwrap();
        let module = ObjectModule::new(builder);
        Self {
            module,
            registry: MachineRegistry::default(),
            ty_map: HashMap::from([
                ("i64".into(), CompilerType::Simple(Type::int(64).unwrap())),
                ("i32".into(), CompilerType::Simple(Type::int(32).unwrap())),
                ("str".into(), CompilerType::Simple(Type::int(64).unwrap())),
            ]),
            rt_funcs: None,
            func_ref_cache: HashMap::new(),
        }
    }
}

impl CompilerCtx {
    // ── Module access ────────────────────────────────────────────────────────

    pub fn module(&self) -> &ObjectModule {
        &self.module
    }

    pub fn module_mut(&mut self) -> &mut ObjectModule {
        &mut self.module
    }

    pub fn finish(self) -> ObjectProduct {
        self.module.finish()
    }

    // ── Machine registry ─────────────────────────────────────────────────────

    pub fn add_machine(&mut self, name: &str) {
        self.registry.add_machine(name);
    }

    pub fn insert_pattern(
        &mut self,
        machine: &str,
        hash: u64,
        param_count: usize,
        branch_id: u128,
        var_count: usize,
    ) -> Result<()> {
        self.registry
            .insert_branch(machine, hash, param_count, branch_id, var_count)
    }

    /// Return the `branch_id` for the branch of `machine` that takes `param_count` parameters.
    pub fn lookup_param_count(&self, machine: &str, param_count: usize) -> Option<u128> {
        self.registry.branch_id_by_param_count(machine, param_count)
    }

    /// Return the variable count for the branch identified by `branch_id`.
    pub fn lookup_vars_count(&self, branch_id: u128) -> Option<usize> {
        self.registry.var_count_by_branch_id(branch_id)
    }

    pub fn machines(&self) -> impl Iterator<Item = &str> {
        self.registry.machine_names()
    }

    // ── Type system ──────────────────────────────────────────────────────────

    pub fn lookup_type(&self, name: &str) -> Option<&CompilerType> {
        self.ty_map.get(name)
    }

    // ── Runtime function handles ─────────────────────────────────────────────

    /// Populate the typed runtime function handles.
    /// Called by `RuntimeBuilder` after all rt functions have been declared.
    pub fn set_rt_funcs(&mut self, rt: RtFuncs) {
        self.rt_funcs = Some(rt);
    }

    pub fn rt_funcs(&self) -> &RtFuncs {
        self.rt_funcs
            .as_ref()
            .expect("Runtime functions not yet initialised")
    }

    // ── FuncRef helper ───────────────────────────────────────────────────────

    /// Call at the start of every new function compilation to reset the
    /// per-function FuncRef cache.  FuncRefs are only valid within the
    /// function they were declared in, so they must not be reused across
    /// function compilations.
    pub fn begin_function(&mut self) {
        self.func_ref_cache.clear();
    }

    /// Get a `FuncRef` for `callee` usable inside the function currently being
    /// built with `builder`. Results are cached per callee within the current
    /// function scope (reset with `begin_function`).
    pub fn get_func(&mut self, builder: &mut FunctionBuilder, callee: &str) -> Result<FuncRef> {
        let key = (String::new(), callee.to_string());

        if let Some(func_ref) = self.func_ref_cache.get(&key) {
            return Ok(*func_ref);
        }

        let func_id = match self.module.get_name(callee) {
            Some(FuncOrDataId::Func(id)) => id,
            _ => bail!("Function '{callee}' is not declared"),
        };

        let func_ref = self.module.declare_func_in_func(func_id, builder.func);
        self.func_ref_cache.insert(key, func_ref);
        Ok(func_ref)
    }

    /// Like `get_func` but takes a `FuncId` directly (no string lookup).
    pub fn declare_func_in_func(
        &mut self,
        func_id: FuncId,
        builder: &mut FunctionBuilder,
    ) -> FuncRef {
        self.module.declare_func_in_func(func_id, builder.func)
    }
}
