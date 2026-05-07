//! `self(...)` state transitions and self-loops.

use super::common::assert_stdout;

#[test]
fn state_transition_via_self() {
    // fsm branch 0: _ -> transitions to branch 1 via self("B").
    // fsm branch 1: n str -> writes n to stdout.
    let src = r#"
        @rt(rt_write)
        fsm is {
          _ -> { self("B") }
          n str -> { rt_write(1 n 1) }
        }
        main is { _ -> { fsm() } }
    "#;
    assert_stdout(src, b"B");
}

#[test]
fn self_loop_terminates() {
    // counter(3): counts down via self(), writes "done" when n == 0.
    let src = r#"
        @rt(rt_write)
        counter is {
          n i64 -> {
            when 0 == n {
              true -> { rt_write(1 "done" 4) }
              false -> { self(n-1) }
            }
          }
        }
        main is { _ -> { counter(3) } }
    "#;
    assert_stdout(src, b"done");
}

#[test]
fn arithmetic_accumulator_in_self_args() {
    // adder(3 0): accumulates 3+2+1=6, checks result with nested when.
    let src = r#"
        @rt(rt_write)
        adder is {
          n i64 acc i64 -> {
            when 0 == n {
              true -> { when acc == 6 {
                true -> { rt_write(1 "ok" 2) }
                false -> { rt_write(1 "fail" 4) }
              } }
              false -> { self(n-1 acc+n) }
            }
          }
        }
        main is { _ -> { adder(3 0) } }
    "#;
    assert_stdout(src, b"ok");
}

#[test]
fn when_after_state_transition() {
    let src = r#"
        @rt(rt_write)
        m is {
          _ -> { self(42) }
          n i64 -> {
            when n == 42 {
              true -> { rt_write(1 "ok" 2) }
              false -> { rt_write(1 "no" 2) }
            }
          }
        }
        main is { _ -> { m() } }
    "#;
    assert_stdout(src, b"ok");
}

#[test]
fn two_concurrent_self_loops() {
    let src = r#"
        @rt(rt_write)
        cnt is {
          n i64 -> {
            when 0 == n {
              true -> { rt_write(1 "x" 1) }
              false -> { self(n-1) }
            }
          }
        }
        main is { _ -> { cnt(2) cnt(2) } }
    "#;
    assert_stdout(src, b"xx");
}
