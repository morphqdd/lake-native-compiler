//! Spawn / process creation tests.

use super::common::{assert_stdout, assert_stdout_contains_all};

#[test]
fn spawn_worker_runs() {
    let src = r#"
        @rt(rt_write)
        worker is { n str -> { rt_write(1 n 2) } }
        main is { _ -> { worker("ok") } }
    "#;
    assert_stdout(src, b"ok");
}

#[test]
fn spawn_two_workers_run() {
    let src = r#"
        @rt(rt_write)
        worker is { n str -> { rt_write(1 n 2) } }
        main is { _ -> { worker("ok") worker("ok") } }
    "#;
    assert_stdout(src, b"okok");
}

#[test]
fn spawn_nested_workers() {
    let src = r#"
        @rt(rt_write)
        worker3 is { n str -> { rt_write(1 n 2) } }
        worker2 is { n str -> { worker3("w3") worker3("w3") rt_write(1 n 2) } }
        main is { _ -> { worker2("w2") } }
    "#;
    assert_stdout_contains_all(src, &["w2", "w3"]);
}

#[test]
fn spawn_three_levels() {
    let src = r#"
        @rt(rt_write)
        worker3 is { n str -> { rt_write(1 n 2) } }
        worker2 is { n str -> { worker3("w3") worker3("w3") rt_write(1 n 2) } }
        worker is { n str -> { rt_write(1 n 2) } }
        main is { _ -> { worker("ok") worker2("w2") } }
    "#;
    assert_stdout_contains_all(src, &["ok", "w2", "w3"]);
}

#[test]
fn forward_ref_worker_declared_after_main() {
    let src = r#"
        @rt(rt_write)
        main is { _ -> { worker("ok") } }
        worker is { n str -> { rt_write(1 n 2) } }
    "#;
    assert_stdout(src, b"ok");
}
