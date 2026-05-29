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

/// Phase 2 — nominal `[]`.  A composite receiver whose type does NOT
/// carry an `is Index` assertion cannot be subscripted: `p[0]` on a
/// plain record must fail to compile (E042).  `index` is a proto
/// method now, not a bare naming convention.
#[test]
fn index_on_non_index_type_errors() {
    let src = r#"
+std.io.{ println }

Pair is { a i64  b i64 }

main is {
  _ -> {
    let p Pair = Pair(1 2)
    let x = p[0]
    println("unreachable")
  }
}
"#;
    let err = try_compile(src).expect_err("expected `[]`-without-Index compile error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("ast") || msg.contains("fail"),
        "unexpected error: {msg}"
    );
}

/// Phase 2 — enforced explicit impl.  An `X is Eq` assertion whose
/// required `eq` machine is absent must fail with E042, even though the
/// impl is verified up front (X never appears in a bounded generic).
/// Also exercises the impl-parser boundary fix: `Color is Eq` followed
/// by `main is { … }` must not swallow `main` as a proto name.
#[test]
fn explicit_impl_missing_method_errors() {
    let src = r#"
+std.io.{ println }

Color is enum { Red Green }

Eq is proto { eq is { Self Self -> i64 } }

Color is Eq

main is {
  _ -> { println("hi") }
}
"#;
    let err = try_compile(src).expect_err("expected missing-impl compile error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("ast") || msg.contains("fail"),
        "unexpected error: {msg}"
    );
}

/// Phase 2 — explicit impl satisfied.  `Color is Eq` with the `eq`
/// machine present compiles and runs.  Guards the impl-parser boundary
/// fix and the relaxed (base-name) method-signature match.
#[test]
fn explicit_impl_satisfied_runs() {
    let src = r#"
+std.io.{ println }

Color is enum { Red Green }

Eq is proto { eq is { Self Self -> i64 } }

eq is {
  a Color b Color -> ret i64 { ret 1 }
}

Color is Eq

main is {
  _ -> { println("impl ok") }
}
"#;
    let out = run(src).expect("compile/run failed");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"impl ok\n");
}

/// Phase 2 — overloaded `index` must not collide in mono (#101).  A user
/// type `Struct[T]` defines its own `index`, and its body subscripts the
/// inner `Vec[T]` (`v[i]` → stdlib `Vec.index`).  Both instantiate at
/// `i64`; before the fix they shared the mangled symbol `index_i64`, so
/// `v[i]` dispatched to `Struct`'s index → E003.  Now owner-qualified.
#[test]
fn overloaded_index_no_mono_collision() {
    ensure_lake_path();
    let src = r#"
+std.io.{ println }
+std.vec.{ Vec vec_new vec_push }
+std.strings.{ int_to_buf }

Struct[T] is { data Vec[T] }
Struct is Index

pub index[T] is {
  st Struct[T] i i64 -> ret T {
    let v Vec[T] = st.data
    let d T = v[i]
    ret d
  }
}

push_into[T] is {
  st Struct[T] val T -> ret Struct[T] {
    let old Vec[T] = st.data
    let v = vec_push(old val)
    ret Struct(v)
  }
}

main is {
  _ -> {
    let v0 Vec[i64] = vec_new()
    let s0 Struct[i64] = Struct(v0)
    let s1 = push_into(s0 10)
    let s2 = push_into(s1 20)
    println(int_to_buf(s2[0]))
    println(int_to_buf(s2[1]))
  }
}
"#;
    let out = run(src).expect("compile/run failed");
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert_eq!(out.stdout, b"10\n20\n");
}
