//! Bug-fix regression tests. Each test pins down a previously-broken behaviour
//! so it cannot silently regress. Add a comment with the symptom + root cause.

use super::common::run;

/// Playground bug: branch dispatch on string literal guards always fell through
/// to the wildcard arm because `dispatch::emit_int_guard_select` only handled
/// `GuardValue::Int`. Both `greeting("Hello, world!")` and `greeting("Some other")`
/// printed "Not hello".
///
/// Status: currently FAILING — fix is on the way. Marked `#[ignore]` until
/// branch dispatch handles `GuardValue::Str` (MPHF + hash-verify, mirroring
/// `when_expr.rs`).
#[test]
#[ignore = "string guard branch dispatch not yet implemented"]
fn str_guard_branch_dispatch_playground_repro() {
    let src = r#"
        @rt(rt_write)
        greeting is {
          "Hello, world!" str -> { rt_write(1 "Hello, world!\n" 14) }
          s str               -> { rt_write(1 "Not hello\n" 10) }
        }
        main is { _ -> {
          greeting("Hello, world!")
          greeting("Some other")
        } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, b"Hello, world!\nNot hello\n");
}
