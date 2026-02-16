use std::collections::HashMap;

use anyhow::{Result, bail};
use cranelift::{
    frontend::Switch,
    prelude::{FunctionBuilder, Type, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::ctx::CompilerCtx;

pub mod jump_expr;
pub mod let_expr;
pub mod num_expr;
pub mod spawn_expr;
pub mod string_expr;
pub mod var_expr;

/// Local variable table for a branch: maps name → (Cranelift type, slot index).
/// The slot index is the position in the runtime variables array.
#[derive(Debug, Default)]
pub struct BranchState {
    vars: HashMap<String, (Type, usize)>,
    /// Lake-level type strings for variables (e.g. "i64", "str", "{}").
    /// Used to resolve the correct type when the frontend emits `{}` for
    /// variables whose type is known from the pattern declaration.
    lake_types: HashMap<String, String>,
    /// Current base slot in JUMP_ARGS for the innermost call being compiled.
    /// Nested calls advance this by the outer call's arg count so that they
    /// write to a disjoint range and never overwrite already-staged args.
    /// This is a compile-time constant captured into `iconst` instructions.
    pub jump_args_base: usize,
}

impl BranchState {
    pub fn get(&self, name: &str) -> Option<(Type, usize)> {
        self.vars.get(name).copied()
    }

    /// Insert a new variable, assigning the next available slot index.
    pub fn insert(&mut self, name: String, ty: Type) -> usize {
        let idx = self.next_index();
        self.vars.insert(name, (ty, idx));
        idx
    }

    /// Insert a variable together with its Lake-level type string.
    pub fn insert_with_lake_type(&mut self, name: String, ty: Type, lake_ty: String) -> usize {
        let idx = self.insert(name.clone(), ty);
        self.lake_types.insert(name, lake_ty);
        idx
    }

    /// Look up the Lake-level type string for a variable by name.
    pub fn lake_type_of(&self, name: &str) -> Option<&str> {
        self.lake_types.get(name).map(|s| s.as_str())
    }

    /// Access the full Lake-type map (name → type string).
    pub fn lake_types(&self) -> &HashMap<String, String> {
        &self.lake_types
    }

    /// Number of variables currently tracked (= number of occupied slots).
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    fn next_index(&self) -> usize {
        self.vars.values().map(|(_, i)| i + 1).max().unwrap_or(0)
    }
}

/// Compile a single expression, appending blocks to `builder` and entries to
/// `branch_switch`. Returns the next `block_id` to use.
pub fn compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    expr: &Expr<'_>,
) -> Result<i64> {
    match expr {
        Expr::Let { ident, ty, default } => {
            let ident_str = ident.inner.to_string();
            let_expr::compile(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                &ident_str,
                &ty.inner,
                default.as_ref().map(|b| &b.inner),
            )
        }
        Expr::String(s, ty) => {
            string_expr::compile(ctx, builder, machine_ctx_var, block_id, branch_switch, s)
        }
        Expr::Jump { ident, args } => {
            let args_inner: Vec<Expr<'_>> = args.iter().map(|a| a.inner.clone()).collect();
            jump_expr::compile(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                &ident.inner,
                &args_inner,
            )
        }
        Expr::Num(n, ty) => {
            num_expr::compile(ctx, builder, machine_ctx_var, block_id, branch_switch, n)
        }
        Expr::Var(v, ty) => var_expr::compile(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            v,
        ),
        _ => bail!("Unsupported expression type: {:?}", expr),
    }
}
