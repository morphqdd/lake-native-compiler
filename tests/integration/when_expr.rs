//! `when` expression: bool, numeric, string discriminants.

use super::common::{assert_stdout, run};

#[test]
fn when_false_branch_runs() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          when false {
            false -> { rt_write(1 "no" 2) }
            true  -> { rt_write(1 "yes" 3) }
          }
        } }
    "#;
    assert_stdout(src, b"no");
}

#[test]
fn when_true_branch_runs() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          when true {
            false -> { rt_write(1 "no" 2) }
            true  -> { rt_write(1 "yes" 3) }
          }
        } }
    "#;
    assert_stdout(src, b"yes");
}

#[test]
fn when_no_match_continues() {
    // No branch matches → silent fallthrough, no output.
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          when 42 { 0 -> { rt_write(1 "zero" 4) } }
        } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, b"");
}

#[test]
fn when_numeric_three_branches() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          when 2 {
            0 -> { rt_write(1 "zero" 4) }
            1 -> { rt_write(1 "one" 3) }
            2 -> { rt_write(1 "two" 3) }
          }
        } }
    "#;
    assert_stdout(src, b"two");
}

#[test]
#[ignore = "BUG: `when buf { \"lit\" -> ... }` over `let buf str = ...` produces no output"]
fn when_string_match() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          let buf str = "lake"
          when buf {
            "hello" -> { rt_write(1 "world\n" 6) }
            "lake"  -> { rt_write(1 "is awesome\n" 11) }
            "lang"  -> { rt_write(1 "is lake\n" 8) }
          }
        } }
    "#;
    assert_stdout(src, b"is awesome\n");
}

#[test]
fn when_string_no_match_silent() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> {
          let buf str = "miss"
          when buf {
            "hello" -> { rt_write(1 "world\n" 6) }
            "lake"  -> { rt_write(1 "is awesome\n" 11) }
          }
        } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, b"");
}
