use std::collections::HashMap;

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::{FuncRef, Function},
    module::{FuncId, FuncOrDataId, Module, default_libcall_names},
    native,
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{Configurable, FunctionBuilder, Type, settings},
};

use crate::compiler::ctx::compiler_type::CompilerType;

pub mod compiler_type;

pub struct CompilerCtx {
    module: ObjectModule,
    machine_map: HashMap<String, HashMap<u64, (usize, u128)>>,
    ty_map: HashMap<String, CompilerType>,
    declared_funcs_in_funcs: HashMap<String, HashMap<String, FuncRef>>,
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
            machine_map: HashMap::new(),
            ty_map: HashMap::from([
                ("i64".into(), CompilerType::Simple(Type::int(64).unwrap())),
                ("str".into(), CompilerType::Simple(Type::int(64).unwrap())),
            ]),
            declared_funcs_in_funcs: HashMap::new(),
        }
    }
}

impl CompilerCtx {
    pub fn module(&self) -> &ObjectModule {
        &self.module
    }

    pub fn module_mut(&mut self) -> &mut ObjectModule {
        &mut self.module
    }

    pub fn finish(self) -> ObjectProduct {
        self.module.finish()
    }

    pub fn add_machine(&mut self, ident: &str) {
        self.machine_map.insert(ident.to_string(), HashMap::new());
    }

    pub fn insert_pattern(
        &mut self,
        ident: &str,
        hash: u64,
        param_count: usize,
        block_id: u128,
    ) -> Result<()> {
        self.machine_map
            .get_mut(ident)
            .ok_or(anyhow::anyhow!("Not found machine with name: {ident}"))?
            .insert(hash, (param_count, block_id));
        Ok(())
    }

    pub fn lookup_param_count(&self, ident: &str, count: usize) -> Option<u128> {
        match self.machine_map.get(ident) {
            Some(machine) => machine
                .values()
                .find(|(param_count, _)| param_count == &count)
                .map(|(_, block_id)| block_id)
                .copied(),
            None => None,
        }
    }

    pub fn machines(&self) -> &HashMap<String, HashMap<u64, (usize, u128)>> {
        &self.machine_map
    }

    pub fn lookup_type(&self, ty: &str) -> Option<&CompilerType> {
        self.ty_map.get(ty)
    }

    pub fn get_func(&mut self, builder: &mut FunctionBuilder, ident: &str) -> Result<FuncRef> {
        let func_id = match self
            .declared_funcs_in_funcs
            .get(&builder.func.name.to_string())
        {
            Some(func_map) => match func_map.get(ident) {
                Some(func_ref) => return Ok(func_ref.clone()),
                None => {
                    let Some(FuncOrDataId::Func(func_id)) = self.module.get_name(ident) else {
                        bail!("Function {ident} is not declare")
                    };
                    func_id
                }
            },
            None => {
                self.declared_funcs_in_funcs
                    .insert(builder.func.name.clone().to_string(), HashMap::new());
                let Some(FuncOrDataId::Func(func_id)) = self.module.get_name(ident) else {
                    bail!("Function {ident} is not declare")
                };
                func_id
            }
        };

        let func_ref = self
            .module_mut()
            .declare_func_in_func(func_id, builder.func);

        if let Some(func_map) = self.declared_funcs_in_funcs.get_mut(ident) {
            func_map.insert(ident.to_string(), func_ref.clone());
        }

        Ok(func_ref)
    }
}
