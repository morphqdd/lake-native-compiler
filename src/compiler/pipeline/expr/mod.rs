use std::collections::HashMap;

use anyhow::{Result, bail};
use cranelift::{
    frontend::Switch,
    prelude::{FunctionBuilder, Type, Variable},
};
use lake_frontend::api::expr::Expr;

use crate::compiler::ctx::CompilerCtx;

/// Outcome of compiling a single expression in the CPS block model.
#[derive(Debug, Clone, Copy)]
pub enum StmtOutcome {
    /// Normal control flow: the caller should continue from this block_id.
    Continue(i64),
    /// `self(...)` state transition: the branch is done and the machine will
    /// be re-entered at block 0 under a new branch_id.
    /// `next_available` is the first block_id not yet claimed by this branch.
    StateChange { next_available: i64 },
}

impl StmtOutcome {
    /// `true` when the expression ends control flow for the current branch.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, StmtOutcome::Continue(_))
    }

    /// First block_id that is free after this expression, regardless of termination.
    pub fn next_available(&self) -> i64 {
        match self {
            StmtOutcome::Continue(id) => *id,
            StmtOutcome::StateChange { next_available } => *next_available,
        }
    }
}

pub mod arith_expr;
pub mod change_state_expr;
pub mod jump_expr;
pub mod let_expr;
pub mod num_expr;
pub mod pure_expr;
pub mod spawn_expr;
pub mod string_expr;
pub mod var_expr;
pub mod when_expr;

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
/// `branch_switch`. Returns a `StmtOutcome` describing control flow.
pub fn compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    expr: &Expr<'_>,
) -> Result<StmtOutcome> {
    if pure_expr::is_pure(expr) {
        return pure_expr::compile(ctx, builder, machine_ctx_var, block_id, branch_switch, state, expr);
    }

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
        Expr::String(s, _ty) => {
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
        Expr::When { cond, branches } => when_expr::compile(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            &cond.inner,
            branches
                .iter()
                .map(|(cond, expr)| {
                    (
                        cond.inner.clone(),
                        expr.iter().map(|expr| expr.inner.clone()).collect(),
                    )
                })
                .collect(),
        ),
        _ => bail!("Unsupported expression type: {:?}", expr),
    }
}
