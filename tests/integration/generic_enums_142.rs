//! Feature #142 — generics phase 4: generic enums.
//!
//! Phase 1 added `[T]` syntax for records; phases 2-3 landed
//! monomorphisation for records + machines.  Phase 4 extends the same
//! pipeline to enums: `Option[T]` and `Result[T E]` mono'd at use
//! sites, then the resolver / typeck / lowering see only concrete
//! `Option_i64`, `Result_i64_buf` etc.
//!
//! See docs/state/features/142_generics.md.

use super::common::assert_stdout;

/// Acceptance: `Option[T]`, `Result[T E]`, and a generic `unwrap_or[T]`
/// machine round-trip end-to-end.
#[test]
fn generic_enums_full_smoke() {
    let src = r#"
+std.io.{ println }

Option[T] is enum {
  Some(T)
  None
}

Result[T E] is enum {
  Ok(T)
  Err(E)
}

unwrap_or[T] is {
  o Option[T] d T -> ret T {
    when o {
      Some(v) -> { ret v }
      None    -> { ret d }
    }
  }
}

main is {
  _ -> {
    let x = Option.Some(42)
    let y Option[i64] = Option.None
    let a = unwrap_or(x 0)
    let b = unwrap_or(y 99)
    when a == 42 {
      true -> { println("ok a") }
      _    -> { println("FAIL a") }
    }
    when b == 99 {
      true -> { println("ok b") }
      _    -> { println("FAIL b") }
    }
    let r Result[i64 buf] = Result.Ok(7)
    when r {
      Ok(n) -> {
        when n == 7 {
          true -> { println("ok r") }
          _    -> { println("FAIL r") }
        }
      }
      Err(_) -> { println("FAIL r") }
    }
  }
}
"#;
    assert_stdout(src, b"ok a\nok b\nok r\n");
}

/// `Result[T E]` standalone — pattern-matches both arms.
#[test]
fn generic_enums_result_only() {
    let src = r#"
+std.io.{ println }

Result[T E] is enum {
  Ok(T)
  Err(E)
}

main is {
  _ -> {
    let r Result[i64 buf] = Result.Ok(11)
    when r {
      Ok(n) -> {
        when n == 11 {
          true -> { println("ok ok") }
          _    -> { println("FAIL ok") }
        }
      }
      Err(_) -> { println("FAIL ok") }
    }
    let e Result[i64 buf] = Result.Err("boom")
    when e {
      Ok(_)  -> { println("FAIL err") }
      Err(_) -> { println("ok err") }
    }
  }
}
"#;
    assert_stdout(src, b"ok ok\nok err\n");
}
