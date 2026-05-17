//! #089 — Phase 1b ANF lifts non-leaf Eq / bitwise / shift operands and
//! `when` discriminants so the backend's `compile_arith` and
//! `pure_expr::compile` never see a composite (Jump / arith / nested
//! compare) in operand position.
//!
//! Before #089 each of these shapes panicked in lowering because Phase
//! 1.5 only lifted plain `Jump` arguments — `when (callee() & 1)` and
//! `(callee() == 0)` slipped through.

use super::common::run;

/// Eq inside a ret-machine arg slot.  `pick(a == b)` would previously
/// route Eq through dispatch's `_ => unsupported` arm because the Eq
/// landed at the top of the spawned arg-staging path.  ANF must hoist
/// the Eq into its own `let __anf_tmp = a == b`.
#[test]
fn anf_eq_in_ret_machine_arg() {
    let src = r#"
        @rt(rt_exit)
        pick is {
          flag i64 -> ret i64 {
            when flag {
              1 -> { ret 7 }
              _ -> { ret 9 }
            }
          }
        }
        main is {
          _ -> {
            let a = 3
            let b = 3
            let r = pick(a == b)
            rt_exit(r)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 7, "stderr: {:?}", out.stderr);
}

/// Bitwise side that is itself a ret-machine call.  The `when`
/// discriminant `(callee() & 1)` previously failed because lift_in_expr
/// didn't descend into `Expr::When { cond, .. }` — callee() stayed
/// unlifted inside cond and pure_expr couldn't fold a non-rt Jump.
#[test]
fn anf_bitwise_with_jump_side_in_when_cond() {
    let src = r#"
        @rt(rt_write)
        @rt(rt_exit)
        odd_seven is {
          _pad i64 -> ret i64 { ret 7 }
        }
        main is {
          _ -> {
            when (odd_seven(0) & 1) {
              1 -> { rt_write(1 "odd" 3) }
              _ -> { rt_write(1 "even" 4) }
            }
            rt_exit(0)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"odd");
}

/// Recursive ANF: `(callee() == 0)` as a stand-alone expression in body
/// position.  Phase 1b lifts the Jump first, then sees `Eq(__lift, 0)`
/// — both leaves, no further work.  Test that the resulting i64 (0 or
/// 1) ends up in TEMP_VAL by feeding it back into rt_exit.
#[test]
fn anf_eq_with_jump_side_recursive() {
    let src = r#"
        @rt(rt_exit)
        zero is {
          _pad i64 -> ret i64 { ret 0 }
        }
        main is {
          _ -> {
            let r = zero(0) == 0
            rt_exit(r)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 1, "stderr: {:?}", out.stderr);
}

/// Bitwise composite in a `when` discriminant with a plain (non-call)
/// non-leaf RHS: `when (a & (b + 1))`.  No ret-machine involved, but
/// the cond is still composite — pre-#089 this worked because both
/// sides were pure-foldable; we keep the test to pin the ANF shape
/// against accidental regressions.
#[test]
fn anf_bitwise_pure_composite_in_when_cond() {
    let src = r#"
        @rt(rt_write)
        @rt(rt_exit)
        main is {
          _ -> {
            let a = 5
            let b = 1
            when (a & (b + 1)) {
              0 -> { rt_write(1 "z" 1) }
              _ -> { rt_write(1 "n" 1) }
            }
            rt_exit(0)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    // 5 & 2 = 0 → first arm.
    assert_eq!(out.stdout, b"z");
}
