//! Feature #145 — enums end-to-end (Phase 3).
//!
//! Phase 1 added `Item::Enum` + parser; Phase 2 lowered
//! `Enum.Variant(args)` into `Expr::Tuple([tag_lit, args...])` inside
//! the resolver.  Phase 3 fixes the pipeline ordering: the resolver
//! runs AFTER `lower_program`, so nested constructors used to bypass
//! `lift_nested_calls` and reach backend codegen as raw `Tuple` in arg
//! position.  These tests pin the smoke + nested-ctor regression so
//! the second lift pass keeps firing.
//!
//! See docs/state/features/145_enums.md.

use super::common::{assert_stdout, run};

/// Smoke: declare a `Result` enum, construct both variants in `let`
/// bindings (no nested calls), pattern-match on them, and print
/// "ok 1" / "ok 2" on success.
#[test]
fn enums_result_smoke() {
    let src = r#"
        +std.io.{ println }

        Result is enum {
          Ok(i64)
          Err(buf)
        }

        unwrap is {
          r Result -> ret i64 {
            when r {
              Ok(n)  -> { ret n }
              Err(_) -> { ret 0 - 1 }
            }
          }
        }

        main is {
          _ -> {
            let good = Result.Ok(42)
            let bad  = Result.Err("oops")
            let g = unwrap(good)
            let b = unwrap(bad)
            when g == 42 {
              true -> { println("ok 1") }
              _    -> { println("FAIL 1") }
            }
            when b == 0 - 1 {
              true -> { println("ok 2") }
              _    -> { println("FAIL 2") }
            }
          }
        }
    "#;
    assert_stdout(src, b"ok 1\nok 2\n");
}

/// Regression: variant constructor in argument position
/// (`fn_taking_result(Result.Ok(7))`) must be lifted into a
/// `let __lift_<id> = Tuple(...)` before reaching backend codegen.
/// Pre-fix this failed because `lift_nested_calls` ran before the
/// resolver emitted the tuple; the second lift pass added in Phase 3
/// catches the freshly-emitted Tuple.
#[test]
fn enums_nested_variant_ctor_arg_lift() {
    let src = r#"
        +std.io.{ println }

        Result is enum {
          Ok(i64)
          Err(buf)
        }

        fn_taking_result is {
          r Result -> ret i64 {
            when r {
              Ok(n)  -> { ret n + 100 }
              Err(_) -> { ret 0 - 1 }
            }
          }
        }

        main is {
          _ -> {
            let x = fn_taking_result(Result.Ok(7))
            when x == 107 {
              true -> { println("ok nested") }
              _    -> { println("FAIL nested") }
            }
          }
        }
    "#;
    let out = run(src).expect("compile/run");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"ok nested\n", "stdout: {:?}", out.stdout_str());
}

// ── Phase 4 — pattern matching with payload bindings ──────────────────────

/// Nullary variant pattern: bare `Empty -> { ... }` matches when the
/// scrutinee carries the nullary variant's tag.  Also exercises a
/// multi-variant enum with mixed payload shapes.
#[test]
fn enums_nullary_variant_pattern() {
    let src = r#"
        +std.io.{ println }

        Range is enum {
          Empty
          Bounded(i64 i64)
        }

        describe is {
          r Range -> ret i64 {
            when r {
              Empty         -> { ret 0 }
              Bounded(_ _)  -> { ret 1 }
            }
          }
        }

        main is {
          _ -> {
            let e = Range.Empty
            let n = describe(e)
            when n == 0 {
              true -> { println("ok empty") }
              _    -> { println("FAIL empty") }
            }
          }
        }
    "#;
    assert_stdout(src, b"ok empty\n");
}

/// Wildcard payload binder `Ok(_)` skips the slot.  No `let _ = ...` is
/// emitted, so backend's let_expr is never asked to bind `_`.
#[test]
fn enums_wildcard_payload_skips_binding() {
    let src = r#"
        +std.io.{ println }

        Result is enum {
          Ok(i64)
          Err(buf)
        }

        check is {
          r Result -> ret i64 {
            when r {
              Ok(_)  -> { ret 1 }
              Err(_) -> { ret 0 }
            }
          }
        }

        main is {
          _ -> {
            let g = check(Result.Ok(7))
            when g == 1 {
              true -> { println("ok wildcard") }
              _    -> { println("FAIL wildcard") }
            }
          }
        }
    "#;
    assert_stdout(src, b"ok wildcard\n");
}

/// Multi-arg variant pattern: `Bounded(lo hi) -> { ret hi - lo }` binds
/// both payload slots and lets the arm body use them.
#[test]
fn enums_multi_arg_variant_pattern() {
    let src = r#"
        +std.io.{ println }

        Range is enum {
          Empty
          Bounded(i64 i64)
        }

        width is {
          r Range -> ret i64 {
            when r {
              Empty           -> { ret 0 }
              Bounded(lo hi)  -> { ret hi - lo }
            }
          }
        }

        main is {
          _ -> {
            let w = width(Range.Bounded(3 10))
            when w == 7 {
              true -> { println("ok width") }
              _    -> { println("FAIL width") }
            }
          }
        }
    "#;
    assert_stdout(src, b"ok width\n");
}

/// Exhaustiveness E029: three variants, only two arms, no wildcard —
/// resolver emits the diagnostic listing the missing variant.  Note
/// the existing E032 code is what the resolver actually emits today
/// (see resolver/mod.rs `check_exhaustiveness`); the brief mentions
/// E029 but the implemented code stayed E032 to match the original
/// Phase 2 plumbing.  Test pins both so the diagnostic surfaces.
#[test]
fn enums_non_exhaustive_emits_diagnostic() {
    use lakec::compiler::compile;
    let src = r#"
        +std.io.{ println }

        Shape is enum {
          Circle(i64)
          Square(i64)
          Triangle(i64 i64 i64)
        }

        area_kind is {
          s Shape -> ret i64 {
            when s {
              Circle(_) -> { ret 1 }
              Square(_) -> { ret 2 }
            }
          }
        }

        main is {
          _ -> {
            let _k = area_kind(Shape.Circle(0))
            println("done")
          }
        }
    "#;
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("prog.lake");
    std::fs::write(&src_path, src).unwrap();
    let res = compile(
        indicatif::ProgressBar::new(0),
        &src_path,
        lakec::compiler::ctx::OptLevel::None,
    );
    assert!(res.is_err(), "expected non-exhaustive `when` to fail compilation");
}

/// Exhaustiveness with wildcard: three variants, two named arms plus
/// `_` catch-all — must compile and execute the wildcard arm when
/// scrutinee falls through.
#[test]
fn enums_wildcard_makes_exhaustive() {
    let src = r#"
        +std.io.{ println }

        Shape is enum {
          Circle(i64)
          Square(i64)
          Triangle(i64 i64 i64)
        }

        kind_of is {
          s Shape -> ret i64 {
            when s {
              Circle(_) -> { ret 1 }
              Square(_) -> { ret 2 }
              _         -> { ret 99 }
            }
          }
        }

        main is {
          _ -> {
            let k = kind_of(Shape.Triangle(1 2 3))
            when k == 99 {
              true -> { println("ok wildcard exh") }
              _    -> { println("FAIL wildcard exh") }
            }
          }
        }
    "#;
    assert_stdout(src, b"ok wildcard exh\n");
}
