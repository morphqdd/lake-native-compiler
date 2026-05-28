//! Feature #142 — generics phases 2-3.
//!
//! Phase 1 added `[T]` parser + AST + registry scaffold; phases 2-3
//! land type-var unification + call-site inference + monomorphisation.
//! See docs/state/features/142_generics.md.

use super::common::{assert_stdout, run};

/// Acceptance: declare a generic `Box[T]` record + a generic `unbox[T]`
/// machine, construct + unwrap an `i64` instance, and print "ok" when
/// the round-trip survives.
#[test]
fn generics_box_unbox_smoke() {
    let src = r#"
+std.io.{ println }

Box[T] is { val T }

unbox[T] is {
  b Box[T] -> ret T {
    ret b.val
  }
}

main is {
  _ -> {
    let b = Box(42)
    let v = unbox(b)
    when v == 42 {
      true -> { println("ok") }
      _    -> { println("FAIL") }
    }
  }
}
"#;
    assert_stdout(src, b"ok\n");
}

/// Two-parameter generic record — verify the compiler accepts and
/// compiles + runs the program without crashing.  Printing is
/// optional per the phase-2/3 brief.
#[test]
fn generics_two_param_record_smoke() {
    let src = r#"
+std.io.{ println }

Pair[K V] is { k K  v V }

main is {
  _ -> {
    let p = Pair(1 "hello")
    println("ok pair")
  }
}
"#;
    let out = run(src).expect("compile/run failed");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"ok pair\n");
}
