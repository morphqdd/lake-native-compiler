use std::collections::HashMap;

use anyhow::Result;
use cranelift::prelude::{FunctionBuilder, Type, Value, Variable};

use crate::compiler::ctx::CompilerCtx;

pub struct ProcessMap {
    inner: HashMap<u32, bool>,
}

impl ProcessMap {
    pub fn new(stack_cap: usize, builder: &mut FunctionBuilder, ty: Type) -> Self {
        let mut inner = HashMap::new();
        (0..stack_cap).for_each(|i| {
            let var_i = builder.declare_var(ty).as_u32();
            if i == 0 {
                inner.insert(var_i, true);
            } else {
                inner.insert(var_i, false);
            }
        });
        Self { inner }
    }
}
