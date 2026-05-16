use std::collections::HashMap;

use anyhow::{Result, bail};
use cranelift::{
    codegen::ir::Block,
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
    StateChange {
        next_available: i64,
    },
    Wait {
        next_avaitlable: i64,
    },
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
            StmtOutcome::Wait { next_avaitlable } => *next_avaitlable,
        }
    }
}

pub mod arith_expr;
pub mod change_state_expr;
pub mod dispatch;
pub mod jump_expr;
pub mod let_expr;
pub mod num_expr;
pub mod pure_expr;
pub mod send_expr;
pub mod spawn_expr;
pub mod string_expr;
pub mod tuple_expr;
pub mod unroll;
pub mod var_expr;
pub mod wait_expr;
pub mod when_expr;

/// Local variable table for a branch: maps name → (Cranelift type, slot index).
/// The slot index is the position in the runtime variables array.
#[derive(Debug, Default)]
pub struct BranchState {
    vars: HashMap<String, (Type, usize)>,
    /// Lake-level type strings for variables (e.g. "i64", "str", "pid").
    /// Used by `jump_expr` as a fallback when the resolver left a
    /// variable reference's type as `Type::Unknown` (rendered "?") —
    /// the BranchState records the type recorded at let / pattern
    /// binding time and we recover it here.
    lake_types: HashMap<String, String>,
    /// Current base slot in JUMP_ARGS for the innermost call being compiled.
    /// Nested calls advance this by the outer call's arg count so that they
    /// write to a disjoint range and never overwrite already-staged args.
    /// This is a compile-time constant captured into `iconst` instructions.
    pub jump_args_base: usize,
    /// Variable cache: maps slot → Cranelift Variable holding the latest
    /// value of that named variable.  Populated by `branch.rs` at branch
    /// entry (loaded from VARIABLES memory) and updated by writes via
    /// `def_var`.  Reads via `pure_expr::fold` check this cache and use
    /// `use_var` (register-resident, no memory load) when present.
    ///
    /// Memory in `VARIABLES[slot*8]` is kept in sync with `def_var`
    /// updates so STOP_LIMIT / STOP_WAIT / STOP_PARK yields don't need
    /// an explicit spill pass — the scheduler resumes correctly via
    /// the existing branch_entry reload.
    cached_vars: HashMap<usize, Variable>,
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

    /// Register a Cranelift Variable as the cache for `slot`.  Called by
    /// `branch.rs` after loading initial values from VARIABLES memory at
    /// branch_entry.  Subsequent reads through `pure_expr::fold` use this
    /// Variable instead of re-loading from memory each time.
    pub fn cache_slot(&mut self, slot: usize, var: Variable) {
        self.cached_vars.insert(slot, var);
    }

    /// Look up the cached Cranelift Variable for `slot`, if any.
    pub fn cached_var(&self, slot: usize) -> Option<Variable> {
        self.cached_vars.get(&slot).copied()
    }

    /// Drop all cached_var entries.  Used by the unroll path after
    /// committing register-resident vars back to memory, so subsequent
    /// compile_expr calls (e.g. exit-arm body) revert to memory loads.
    pub fn clear_var_cache(&mut self) {
        self.cached_vars.clear();
    }

    /// Drop the slot bound to `name` if any.  Used by `unroll.rs` to
    /// scope let-bindings introduced inside an unrolled iteration —
    /// each iter declares a fresh `let s1 = ...`, processes its
    /// dependents, and then removes the binding before the next iter
    /// re-introduces the same name (with a different Cranelift
    /// Variable).
    ///
    /// Does NOT renumber surviving slots; the freed slot id remains
    /// unused for the rest of the branch.  Callers that care about
    /// dense slot allocation must arrange their inserts in stable
    /// order outside the scoped region.
    pub fn remove(&mut self, name: &str) {
        if let Some((_, slot)) = self.vars.remove(name) {
            self.cached_vars.remove(&slot);
        }
        self.lake_types.remove(name);
    }
}

/// #80 Level 2 — does `expr` accept a precreated entry block?
///
/// Only handlers that can fold the caller-supplied entry block into
/// their own first-block emit can accept entry: pure_expr and the Level
/// 1 pure-default let path.
///
/// Other handlers (non-pure let, when, wait, jump, spawn, self) create
/// their own first block internally and can't be re-targeted.
fn accepts_entry(expr: &Expr<'_>) -> bool {
    if pure_expr::is_pure(expr) {
        return true;
    }
    if let Expr::Let {
        default: Some(d), ..
    } = expr
    {
        return pure_expr::is_pure(&d.inner);
    }
    false
}

/// Public wrapper for use by branch.rs / wait_expr's super-block grouping.
pub fn accepts_entry_pub(expr: &Expr<'_>) -> bool {
    accepts_entry(expr)
}

/// #80 Level 2 — does `expr` emit a fall_through brif at its exit?
///
/// Broader than `accepts_entry`: any let (pure or impure default) can
/// emit fall_through at its FINAL save block, even when its internal
/// sub-blocks still go through qb.
///
/// We chain only when the current statement emits AND the next accepts.
fn emits_fall_through(expr: &Expr<'_>) -> bool {
    if pure_expr::is_pure(expr) {
        return true;
    }
    matches!(expr, Expr::Let { .. })
}

/// Combined predicate: the (i, i+1) boundary is fast-path-chainable iff
/// iter i emits fall_through AND iter i+1 accepts entry.  Used by
/// branch.rs to decide whether to allocate a fall_through block.
pub fn is_fast_chain_pair(this: &Expr<'_>, next: &Expr<'_>) -> bool {
    emits_fall_through(this) && accepts_entry(next)
}

/// Backward-compatible single-expr predicate, retained for diagnostics.
/// Reports true when the expression participates in any fast-path role
/// (entry or exit).
pub fn is_fast_path_eligible(expr: &Expr<'_>) -> bool {
    emits_fall_through(expr) || accepts_entry(expr)
}

/// Compile a single expression, appending blocks to `builder` and entries to
/// `branch_switch`. Returns a `StmtOutcome` describing control flow.
///
/// `entry` (#80 Level 2) — when `Some(b)`, the handler emits its first
/// instructions into `b` instead of creating its own first block.
/// Used by branch.rs to chain fast-path statements without an
/// intermediate scheduler round-trip.
///
/// `fall_through` (#80 Level 2) — when `Some(b)`, a fast-path handler
/// emits `dec quantum; brif zero, fast_yield[next], b` at its exit.
/// Handlers that don't support fast-path ignore this.
pub fn compile_expr(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &mut BranchState,
    expr: &Expr<'_>,
    entry: Option<Block>,
    fall_through: Option<Block>,
    omit_exit: bool,
) -> Result<StmtOutcome> {
    if pure_expr::is_pure(expr) {
        return pure_expr::compile(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            expr,
            entry,
            fall_through,
            omit_exit,
        );
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
                entry,
                fall_through,
                omit_exit,
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
        Expr::Wait { handlers, filter } => wait_expr::compile(
            ctx,
            builder,
            machine_ctx_var,
            block_id,
            branch_switch,
            state,
            handlers.iter().map(|branch| branch.inner.clone()).collect(),
            filter.iter().map(|f| f.inner.clone()).collect(),
        ),
        Expr::Tuple(elems) => {
            let inner: Vec<Expr<'_>> = elems.iter().map(|e| e.inner.clone()).collect();
            tuple_expr::compile(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                &inner,
            )
        }
        Expr::Index { receiver, index } => {
            // `buf[i]` lowers to a `rt_load_u8` call internally — the
            // language exposes no first-class index op at codegen, but
            // we avoid forcing the user to `@rt(rt_load_u8)`.  The
            // runtime always emits rt_load_u8 regardless of directives;
            // we synthesize a Jump and route through the standard
            // jump_expr machinery so arg staging + return-via-TEMP_VAL
            // work uniformly.
            use chumsky::span::Spanned;
            use lake_frontend::api::ast::{Ident, Type as AstType};
            let span: chumsky::span::SimpleSpan = (0..0).into();
            let callee = Expr::Var(
                "rt_load_u8",
                AstType::Named(Spanned {
                    inner: Ident::new("rt_load_u8"),
                    span,
                }),
            );
            ctx.declare_rt_func_in_prog("rt_load_u8");
            jump_expr::compile(
                ctx,
                builder,
                machine_ctx_var,
                block_id,
                branch_switch,
                state,
                &callee,
                &[receiver.inner.clone(), index.inner.clone()],
            )
        }
        _ => bail!("Unsupported expression type: {:?}", expr),
    }
}
