//! Rust-style `panic` + `assert` with compiler-injected call-site
//! location (track_caller MVP).
//!
//! `panic(msg)` prints `lake: panicked at <file>:<line>:<col>: <msg>` to
//! stderr and aborts the whole process with exit code 101 (Rust's panic
//! convention).  The `<file>:<line>:<col>` prefix is injected by the
//! frontend pass `panic_loc` — users never write it.
//!
//! See lake-frontend/src/panic_loc.rs and std/panic.lake.

use super::common::run;

/// Point the loader at the real stdlib (`+std.panic`).  Env is
/// process-global across the shared test binary; mirrors the protocols
/// harness.
fn ensure_lake_path() {
    unsafe {
        std::env::set_var("LAKE_PATH", "/home/morphe/compiler/lake-stdlib");
    }
}

/// `panic("msg")` → stderr carries the location + message, exit 101.
#[test]
fn panic_prints_location_and_aborts_101() {
    ensure_lake_path();
    let src = r#"
+std.panic.{ panic }

main is {
  _ -> {
    panic("boom")
  }
}
"#;
    let out = run(src).expect("compile/link/run failed");
    assert_eq!(out.exit_code, 101, "stdout: {:?}", out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("panicked at"), "stderr: {err}");
    assert!(err.contains("prog.lake:"), "stderr should name the file: {err}");
    assert!(err.contains("boom"), "stderr should carry the message: {err}");
    // The `panic("boom")` statement sits on source line 6.
    assert!(err.contains(":6:"), "stderr should carry the line: {err}");
}

/// `assert(cond msg)` with a false condition panics (exit 101) and
/// reports the assert call site; a true condition is a no-op.
#[test]
fn assert_false_panics_true_is_noop() {
    ensure_lake_path();
    let fail = r#"
+std.panic.{ assert }

main is {
  _ -> {
    pin assert(0 "must hold")
  }
}
"#;
    let out = run(fail).expect("compile/link/run failed");
    assert_eq!(out.exit_code, 101, "stdout: {:?}", out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("must hold"), "stderr: {err}");
    assert!(err.contains("panicked at"), "stderr: {err}");

    let ok = r#"
+std.io.{ println }
+std.panic.{ assert }

main is {
  _ -> {
    pin assert(1 "always true")
    println("survived")
  }
}
"#;
    let out2 = run(ok).expect("compile/link/run failed");
    assert_eq!(out2.exit_code, 0, "stderr: {:?}", out2.stderr);
    assert_eq!(out2.stdout, b"survived\n");
}
