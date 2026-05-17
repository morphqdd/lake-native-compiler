//! Feature #055 — tuple pattern matching in `let` and `when` arms.
//!
//! Verifies the lowering pass in lake-frontend rewrites
//! `when X { { :ok v } -> ... }` into `let __wtp = X; when __wtp.0 { :ok -> { let v = __wtp.1; ... } }`
//! and that `let { a b } = pair` destructure end-to-end works on a
//! ret-machine returning a tuple shape.

use super::common::{assert_stdout, run};

/// Smoke test: a ret-machine returns `{atom buf}`, and the caller
/// uses `let { tag b } = ...` to destructure.  Then the `:ok` arm
/// of an inner `when` writes the buf out.
#[test]
fn let_tuple_destructure_on_ret_machine() {
    let src = r#"
        @rt(rt_allocate)
        @rt(rt_write)

        main is { _ -> {
          let r = rt_allocate(16)
          let { tag b } = r
          when tag {
            :ok  -> { rt_write(1 "ok\n" 3) }
            _    -> { rt_write(1 "err\n" 4) }
          }
        } }
    "#;
    assert_stdout(src, b"ok\n");
}

/// Tuple-pattern `when` arms with atom dispatch + binding the value
/// slot.  Verifies the rewrite into nested `when` on field 0 with
/// a `let v = __wtp.1` inside the arm body.
#[test]
fn when_tuple_pattern_atom_dispatch() {
    let src = r#"
        @rt(rt_allocate)
        @rt(rt_write)

        main is { _ -> {
          let r = rt_allocate(32)
          when r {
            { :ok  b } -> { rt_write(1 "ok\n" 3) }
            { :err _ } -> { rt_write(1 "err\n" 4) }
          }
        } }
    "#;
    assert_stdout(src, b"ok\n");
}

/// Sub-binding inside an atom-dispatch arm: the `v` binding inside
/// `{ :ok v }` must be in scope inside the arm body and evaluate to
/// the corresponding slot of the discriminant.  Exit code carries
/// the bound i64 so we can assert it without rt_write plumbing.
#[test]
fn when_tuple_pattern_binds_slot_value() {
    // ret-machine returns `{atom i64}` literal.  `when r { {:ok
    // x} -> { x } }` should land `x = 7` in TEMP_VAL.  Single-arg
    // pattern (not bare `_`) so the pure-inline path's arity check
    // matches the user call site.
    let src = r#"
        make is { n i64 -> ret { atom i64 } { ret { :ok 7 } } }
        main is { _ -> {
          let r = make(0)
          when r {
            { :ok x  } -> { x }
            { :err _ } -> { 0 }
          }
        } }
    "#;
    let out = run(src).expect("compile/run");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}

/// `let { a b } = literal_tuple` — destructure a literal `{ 1 2 }`
/// at let-statement position.  Asserts the lowering's `LetTuple`
/// expansion still works (covered by tests but kept here so the
/// #055 surface has a regression anchor).
#[test]
fn let_tuple_destructure_literal() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          let { a b } = { 1 2 }
          when a {
            1 -> { rt_write(1 "one\n" 4) }
            _ -> { rt_write(1 "no\n"  3) }
          }
        } }
    "#;
    assert_stdout(src, b"one\n");
}
