//! Branch literal guards on call-site dispatch.
//!
//! See also `regression.rs` for str-guard playground bug coverage.

use super::common::{assert_stdout, assert_stdout_contains_all};

#[test]
fn int_guard_three_way_dispatch() {
    // fib-style: three branches sharing the same i64 type hash,
    // dispatched by literal guard value at call site.
    let src = r#"
        @rt(rt_write)
        fib is {
          0 i64 -> { rt_write(1 "zero" 4) }
          1 i64 -> { rt_write(1 "one" 3) }
          n i64 -> { rt_write(1 "other" 5) }
        }
        main is { _ -> { fib(0) fib(1) fib(2) } }
    "#;
    assert_stdout_contains_all(src, &["zero", "one", "other"]);
}

#[test]
fn int_guard_self_transition() {
    // self() with literal guard: 0->done, n-> count down via self.
    let src = r#"
        @rt(rt_write)
        counter is {
          0 i64 -> { rt_write(1 "done" 4) }
          n i64 -> { self(n-1) }
        }
        main is { _ -> { counter(3) } }
    "#;
    assert_stdout(src, b"done");
}

#[test]
fn int_guard_single_match_no_wildcard() {
    // No wildcard branch — calling with unmatched value should silently
    // fall through (no panic, no output).
    let src = r#"
        @rt(rt_write)
        f is {
          1 i64 -> { rt_write(1 "one" 3) }
          2 i64 -> { rt_write(1 "two" 3) }
        }
        main is { _ -> { f(1) } }
    "#;
    assert_stdout(src, b"one");
}

#[test]
fn int_guard_negative_value() {
    let src = r#"
        @rt(rt_write)
        f is {
          0  i64 -> { rt_write(1 "zero" 4) }
          n  i64 -> { rt_write(1 "neg"  3) }
        }
        main is { _ -> { f(-7) } }
    "#;
    assert_stdout(src, b"neg");
}
