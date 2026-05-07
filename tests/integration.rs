//! Integration test entry point.
//!
//! Cargo treats every top-level `.rs` in `tests/` as a separate test binary.
//! Keeping a single `integration.rs` and putting all topic modules under
//! `tests/integration/*` means one shared compile unit with parallel-by-default
//! test execution inside.

#[macro_use]
mod integration {
    pub mod common;

    pub mod basics;
    pub mod guards;
    pub mod messaging;
    pub mod regression;
    pub mod self_call;
    pub mod spawn;
    pub mod when_expr;
}
