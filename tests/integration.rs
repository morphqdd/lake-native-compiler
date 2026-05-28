//! Integration test entry point.
//!
//! Cargo treats every top-level `.rs` in `tests/` as a separate test binary.
//! Keeping a single `integration.rs` and putting all topic modules under
//! `tests/integration/*` means one shared compile unit with parallel-by-default
//! test execution inside.

#[macro_use]
mod integration {
    pub mod common;

    pub mod anf_eq_bitwise;
    pub mod basics;
    pub mod bug_120;
    pub mod enums_145;
    pub mod generic_enums_142;
    pub mod generics_142;
    pub mod guards;
    pub mod mangling;
    pub mod messaging;
    pub mod regression;
    pub mod self_call;
    pub mod slab_route_150;
    pub mod smoke_099;
    pub mod spawn;
    pub mod tuple_pattern;
    pub mod when_expr;
}
