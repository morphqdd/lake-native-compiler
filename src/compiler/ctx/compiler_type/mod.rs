use cranelift::prelude::Type;

#[derive(Clone)]
pub enum CompilerType {
    Simple(Type),
    Complex(Vec<Type>),
}

impl CompilerType {
    pub fn unwrap_simple(self) -> Type {
        match self {
            Self::Simple(ty) => ty,
            _ => panic!("Type is not a simple"),
        }
    }
}
