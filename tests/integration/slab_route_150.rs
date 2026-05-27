//! #150 phase 4 — rt_allocate / rt_free routed through the slab path.
//!
//! Sets `LAKE_SLAB_ALLOC=1` at the start of each test so the compiler
//! emits the slab-routed IR for the user-facing allocator.  The tests
//! exercise the same surface as the bucket allocator — a basic exec
//! that allocates and frees — and confirms the program runs to clean
//! exit.
//!
//! These tests must run single-threaded (cargo test --test-threads=1
//! at the suite level OR rely on the test name being uniquely scheduled
//! by the harness).  We use a process-wide mutex to serialise within
//! this module so parallel tests outside slab mode are unaffected.

use std::sync::Mutex;

use super::common::run;

static SLAB_LOCK: Mutex<()> = Mutex::new(());

fn with_slab_mode<R>(f: impl FnOnce() -> R) -> R {
    // Lock for the duration of compile+run so other tests in this
    // process don't see our env var.  Other test modules don't read
    // LAKE_SLAB_ALLOC, so cross-module parallelism is fine — only
    // intra-module ordering matters.
    let _guard = SLAB_LOCK.lock().unwrap();
    let prev = std::env::var("LAKE_SLAB_ALLOC").ok();
    unsafe {
        std::env::set_var("LAKE_SLAB_ALLOC", "1");
    }
    let out = f();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("LAKE_SLAB_ALLOC", v),
            None => std::env::remove_var("LAKE_SLAB_ALLOC"),
        }
    }
    out
}

/// Smallest non-trivial Lake program — exits 0.  Verifies the compiler
/// pipeline produces valid slab-routed IR for a no-alloc-required main.
#[test]
fn slab_route_main_exits_zero() {
    with_slab_mode(|| {
        let src = r#"
            main is { _ -> {} }
        "#;
        let out = run(src).unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    });
}

/// Allocate a small buffer via rt_allocate, write to it, exit.  The
/// allocation comes from the slab path; the program must not crash on
/// the header-write or the implicit rt_free on actor death.
#[test]
fn slab_route_small_alloc_runs() {
    with_slab_mode(|| {
        let src = r#"
            @rt(rt_allocate)
            @rt(rt_write)

            main is {
              _ -> {
                let r = rt_allocate(32)
                when r.0 {
                  :ok -> { rt_write(1 "ok\n" 3) }
                  _   -> { rt_write(1 "err\n" 4) }
                }
              }
            }
        "#;
        let out = run(src).unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
        assert_eq!(out.stdout_str(), "ok\n");
    });
}

/// Many alloc+free cycles within a single actor — exercises slab
/// bitmap reuse, free_count tracking, and slab reclamation.  If the
/// slab path has any off-by-one or arithmetic bug, this test crashes.
#[test]
fn slab_route_many_alloc_free_cycles() {
    with_slab_mode(|| {
        let src = r#"
            @rt(rt_allocate)
            @rt(rt_free)
            @rt(rt_write)

            loop is {
              0 i64 -> { rt_write(1 "ok\n" 3) }
              n i64 -> {
                let r = rt_allocate(128)
                when r.0 {
                  :ok -> {
                    rt_free(r.1)
                    self(n - 1)
                  }
                  _ -> { rt_write(1 "err\n" 4) }
                }
              }
            }

            main is { _ -> { loop(200) } }
        "#;
        let out = run(src).unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
        assert_eq!(out.stdout_str(), "ok\n");
    });
}

/// Allocate + explicit free round-trip.  Exercises the slab-routed
/// rt_free path: chunk address recovered by `chunk & !(SLAB_SIZE-1)`,
/// bitmap bit set, free_count incremented.  Reusing the same chunk
/// would also exercise the slab path's bit-clear-on-alloc.
#[test]
fn slab_route_alloc_free_alloc() {
    with_slab_mode(|| {
        let src = r#"
            @rt(rt_allocate)
            @rt(rt_free)
            @rt(rt_write)

            main is {
              _ -> {
                let r1 = rt_allocate(64)
                when r1.0 {
                  :ok -> {
                    rt_free(r1.1)
                    let r2 = rt_allocate(64)
                    when r2.0 {
                      :ok -> { rt_write(1 "ok\n" 3) }
                      _   -> { rt_write(1 "err2\n" 5) }
                    }
                  }
                  _ -> { rt_write(1 "err1\n" 5) }
                }
              }
            }
        "#;
        let out = run(src).unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
        assert_eq!(out.stdout_str(), "ok\n");
    });
}
