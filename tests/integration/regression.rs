//! Bug-fix regression tests. Each test pins down a previously-broken behaviour
//! so it cannot silently regress. Add a comment with the symptom + root cause.

use super::common::run;

/// Playground bug: branch dispatch on string literal guards always fell through
/// to the wildcard arm because `dispatch::emit_int_guard_select` only handled
/// `GuardValue::Int`. Both `greeting("Hello, world!")` and `greeting("Some other")`
/// printed "Not hello".
///
/// Fixed by `dispatch::emit_str_guard_select`: MPHF + hash-verify + memcmp
/// path covering string-guarded branches at any call site (spawn / self / send).
#[test]
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
    let s = out.stdout_str();
    // Both spawned actors must run.  Order between them is round-robin and
    // therefore not guaranteed — assert presence of both branches.
    assert!(
        s.contains("Hello, world!"),
        "first-branch output missing: {s:?}"
    );
    assert!(s.contains("Not hello"), "fallback-branch output missing: {s:?}");
}
