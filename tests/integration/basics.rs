//! Smallest end-to-end programs: hello world, empty main, escapes.

use super::common::{assert_stdout, run};

#[test]
fn hello_world_exits_zero() {
    let src = r#"@rt(rt_write) main is { _ -> { rt_write(1 "Hello, world!" 13) } }"#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0);
}

#[test]
fn string_escape_newline() {
    let src = r#"@rt(rt_write) main is { _ -> { rt_write(1 "ok\n" 3) } }"#;
    assert_stdout(src, b"ok\n");
}

#[test]
fn empty_main_exits_zero() {
    let src = r#"main is { _ -> { } }"#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0);
}

#[test]
fn paren_grouping_in_expression() {
    // Without grouping, `2 * 3 + 4` is `(2*3)+4 = 10`.  With grouping
    // `2 * (3 + 4)` is 14 — the test exit-codes the difference so a
    // regression in the new `(expr)` atom is impossible to miss.
    let src = r#"
        @rt(rt_exit)
        main is { _ -> {
          let a = 2 * (3 + 4)
          rt_exit(a)
        } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 14);
}
