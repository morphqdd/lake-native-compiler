//! `wait` / message passing tests.

use super::common::{assert_stdout, assert_stdout_contains_all};

#[test]
fn wait_and_send_single_message() {
    let src = r#"
        @rt(rt_write)
        receiver is { _ -> { wait { n i64 -> { rt_write(1 "ok" 2) } } } }
        main is { _ -> { let p pid = receiver() p(42) } }
    "#;
    assert_stdout(src, b"ok");
}

#[test]
fn wait_loop_multiple_messages() {
    // receiver loops via self() to wait for 3 messages, prints "." for each.
    let src = r#"
        @rt(rt_write)
        receiver is {
          remaining i64 -> {
            when 1 <= remaining {
              true -> { wait { n i64 -> {
                rt_write(1 "." 1)
                self(remaining-1)
              } } }
            }
          }
        }
        sender is {
          target pid count i64 -> {
            when 1 <= count {
              true -> { target(1) self(target count-1) }
            }
          }
        }
        main is { _ -> { let r pid = receiver(3) sender(r 3) } }
    "#;
    assert_stdout(src, b"...");
}

#[test]
fn ping_pong_via_pid_in_mailbox() {
    let src = r#"
        @rt(rt_write)
        ponger is { _ -> { wait { partner pid -> {
          rt_write(1 "A" 1) partner(1)
        } } } }
        pinger is { partner pid -> {
          partner(1)
          wait { n i64 -> { rt_write(1 "B" 1) } }
        } }
        main is { _ -> {
          let po pid = ponger()
          let pi pid = pinger(po)
          po(pi)
        } }
    "#;
    assert_stdout(src, b"AB");
}

#[test]
fn ping_pong_multi_round() {
    let src = r#"
        @rt(rt_write)
        ponger is {
          _ -> { wait { partner pid -> { self(partner 3) } } }
          partner pid remaining i64 -> {
            when 1 <= remaining {
              true -> { wait { n i64 -> {
                partner(1) self(partner remaining-1)
              } } }
              false -> { rt_write(1 "X" 1) }
            }
          }
        }
        pinger is {
          partner pid remaining i64 -> {
            when 1 <= remaining {
              true -> { partner(1) wait { n i64 -> {
                self(partner remaining-1)
              } } }
              false -> { rt_write(1 "Y" 1) }
            }
          }
        }
        main is { _ -> {
          let po pid = ponger()
          let pi pid = pinger(po 3)
          po(pi)
        } }
    "#;
    assert_stdout(src, b"XY");
}

#[test]
fn wait_int_guard_dispatch() {
    // Wait handlers with i64 literal guards + wildcard fallback.
    let src = r#"
        @rt(rt_write)
        receiver is { _ -> { wait {
          0 i64 -> { rt_write(1 "zero" 4) }
          1 i64 -> { rt_write(1 "one" 3) }
          n i64 -> { rt_write(1 "other" 5) }
        } } }
        sender is { target pid val i64 -> { target(val) } }
        main is { _ -> {
          let r0 pid = receiver()
          let r1 pid = receiver()
          let r2 pid = receiver()
          sender(r0 0) sender(r1 1) sender(r2 2)
        } }
    "#;
    assert_stdout_contains_all(src, &["zero", "one", "other"]);
}
