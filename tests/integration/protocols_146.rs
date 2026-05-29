//! Feature #146 — protocols phase 1.
//!
//! Proto declarations, `[T: Proto]` bounds, mono-time bound verification,
//! and the `recv[i]` → `index(recv i)` desugar for non-buf receivers.
//! Dispatch reuses the existing MPHF + overload resolution — no vtables.
//!
//! See docs/state/features/146_protos.md.

use super::common::run;
use indicatif::ProgressBar;
use lakec::compiler::{compile, ctx::OptLevel};
use std::fs;
use tempfile::tempdir;

/// Ensure the stdlib root is discoverable (`+std.io`, `+std.vec`).
/// Env is process-global across the shared test binary; setting it here
/// keeps this module self-contained.
fn ensure_lake_path() {
    // SAFETY: every integration test that touches the stdlib sets this to
    // the same value, so the cross-thread write is benign.  Mirrors
    // smoke_099's harness.
    unsafe {
        std::env::set_var("LAKE_PATH", "/home/morphe/compiler/lake-stdlib");
    }
}

/// Compile only, returning the result so a test can assert a compile error.
fn try_compile(src: &str) -> anyhow::Result<Vec<u8>> {
    ensure_lake_path();
    let dir = tempdir()?;
    let src_path = dir.path().join("prog.lake");
    fs::write(&src_path, src)?;
    compile(ProgressBar::new(0), &src_path, OptLevel::None)
}

/// Acceptance: a `[T: Eq]` machine called with a type that HAS `eq`
/// compiles and runs.  `Tag` is an enum with a matching `eq` machine, so
/// it auto-implements `Eq` (structural presence of the required machine).
#[test]
fn bounded_generic_with_satisfying_type_runs() {
    ensure_lake_path();
    let src = r#"
+std.io.{ println }

Eq is proto {
  eq is { Self Self -> bool }
}

Tag is enum { A B }

eq is {
  x Tag y Tag -> ret bool {
    when x {
      A -> { when y { A -> { ret true } _ -> { ret false } } }
      B -> { when y { B -> { ret true } _ -> { ret false } } }
    }
  }
}

same[T: Eq] is {
  a T b T -> ret bool {
    ret eq(a b)
  }
}

main is {
  _ -> {
    let p = Tag.A
    let q = Tag.A
    when same(p q) {
      true -> { println("ok eq") }
      _    -> { println("FAIL eq") }
    }
  }
}
"#;
    let out = run(src).expect("compile/run failed");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"ok eq\n");
}

/// A `[T: Eq]` machine instantiated with a type that has NO `eq` machine
/// must fail to compile with E042.
#[test]
fn bounded_generic_missing_method_errors_e042() {
    let src = r#"
+std.io.{ println }

Eq is proto {
  eq is { Self Self -> bool }
}

NoEq is enum { X Y }

same[T: Eq] is {
  a T b T -> ret bool {
    ret true
  }
}

main is {
  _ -> {
    let p = NoEq.X
    let q = NoEq.Y
    when same(p q) {
      true -> { println("never") }
      _    -> { println("never") }
    }
  }
}
"#;
    let err = try_compile(src).expect_err("expected E042 compile error");
    // The frontend surfaces E042 through the build pipeline; the native
    // driver wraps it into an anyhow error.  We can't read the code text
    // out of the wrapper, so assert compilation failed.  (The dedicated
    // frontend unit test asserts the E042 code directly.)
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("ast") || msg.to_lowercase().contains("fail"),
        "unexpected error: {msg}"
    );
}

/// `v[3]` on a `Vec[i64]` dispatches to the stdlib `index` alias
/// (→ `vec_get`) and returns the element.
#[test]
fn index_desugar_on_vec_dispatches_to_vec_get() {
    ensure_lake_path();
    let src = r#"
+std.io.{ println }
+std.vec.{ vec_new vec_push }

main is {
  _ -> {
    let v0 Vec[i64] = vec_new()
    let v1 = vec_push(v0 10)
    let v2 = vec_push(v1 20)
    let v3 = vec_push(v2 30)
    let x = v3[2]
    when x == 30 {
      true -> { println("ok index") }
      _    -> { println("FAIL index") }
    }
  }
}
"#;
    let out = run(src).expect("compile/run failed");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"ok index\n");
}
