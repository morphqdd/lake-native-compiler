use cranelift::{
    module::default_libcall_names,
    native,
    object::{ObjectBuilder, ObjectModule, ObjectProduct},
    prelude::{Configurable, settings},
};

pub struct CompilerCtx {
    module: ObjectModule,
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
        Self { module }
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
}
