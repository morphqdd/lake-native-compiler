use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::FuncRef,
    module::{FuncId, FuncOrDataId, Linkage, Module, default_libcall_names},
    native,
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{AbiParam, Block, Configurable, FunctionBuilder, Type, settings},
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
    declared_in_prog_rt_func: BTreeSet<String>,
    current_machine: Option<String>,
    /// The block inside the current machine function that CPS blocks jump to
    /// instead of returning directly.  Set by `compile_machine` before branch
    /// compilation; cleared by `begin_function`.
    quantum_block: Option<Block>,
}

/// Cranelift optimisation level.
#[derive(Debug, Clone, Copy, Default)]
pub enum OptLevel {
    #[default]
    None,
    Speed,
    SpeedAndSize,
}

impl OptLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
        }
    }
}

impl Default for CompilerCtx {
    fn default() -> Self {
        Self::new(OptLevel::None)
    }
}

impl CompilerCtx {
    pub fn new(opt: OptLevel) -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", opt.as_str()).unwrap();
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
            declared_in_prog_rt_func: BTreeSet::new(),
            current_machine: None,
            quantum_block: None,
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

    /// O(1) dispatch lookup: hash(arg_types) → (branch_id, var_count, param_count).
    pub fn lookup_branch_by_hash(&self, machine: &str, hash: u64) -> Option<(u128, usize, usize)> {
        self.registry.branch_by_hash(machine, hash)
    }

    /// Return the variable count for the branch identified by `branch_id` within `machine`.
    pub fn lookup_vars_count(&self, machine: &str, branch_id: u128) -> Option<usize> {
        self.registry.var_count_by_branch_id(machine, branch_id)
    }

    /// Return the maximum variable count across all branches of `machine`.
    /// Used to size the VARIABLES buffer so any state transition is safe.
    pub fn max_branch_var_count(&self, machine: &str) -> Option<usize> {
        self.registry.max_var_count(machine)
    }

    /// Return the pre-computed pattern hash for a branch (set during the index pre-pass).
    pub fn get_branch_hash(&self, machine: &str, branch_id: u128) -> Option<u64> {
        self.registry.hash_by_branch_id(machine, branch_id)
    }

    /// Update the exact variable count for a branch after it has been compiled.
    pub fn update_branch_var_count(&mut self, machine: &str, branch_id: u128, var_count: usize) {
        self.registry.update_var_count(machine, branch_id, var_count);
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
        self.quantum_block = None;
    }

    /// Set the quantum continuation block for the current machine function.
    /// CPS blocks jump here instead of returning directly to the scheduler.
    pub fn set_quantum_block(&mut self, block: Block) {
        self.quantum_block = Some(block);
    }

    /// Get the quantum continuation block.  Panics if called outside a machine
    /// function compilation (i.e. before `set_quantum_block`).
    pub fn quantum_block(&self) -> Block {
        self.quantum_block
            .expect("quantum_block not set — call set_quantum_block before compiling branches")
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

    pub fn declare_rt_func_in_prog(&mut self, func_name: &str) {
        self.declared_in_prog_rt_func.insert(func_name.to_string());
    }

    pub fn is_declared_rt_func_in_prog(&self, func_name: &str) -> bool {
        self.declared_in_prog_rt_func.contains(func_name)
    }

    pub fn get_registry(&self) -> &MachineRegistry {
        &self.registry
    }

    pub fn set_current_machine(&mut self, name: Option<String>) {
        self.current_machine = name;
    }

    pub fn get_current_machine(&mut self) -> Option<String> {
        self.current_machine.clone()
    }

    /// Pre-declare a machine's Cranelift function before code generation.
    ///
    /// All machine functions share the same signature `fn(ctx_fat_ptr: ptr) -> ptr`.
    /// Calling this before the main compilation pass allows any branch to reference
    /// any machine regardless of declaration order in the source file.
    pub fn predeclare_machine(&mut self, name: &str) -> anyhow::Result<()> {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut module_ctx = self.module.make_context();
        module_ctx.func.signature.params.push(AbiParam::new(ptr_ty));
        module_ctx.func.signature.returns.push(AbiParam::new(ptr_ty));
        let sig = module_ctx.func.signature.clone();
        self.module.declare_function(name, Linkage::Export, &sig)?;
        self.module.clear_context(&mut module_ctx);
        Ok(())
    }
}
