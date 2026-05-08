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

/// Allocator regression: spawn many short-lived actors in sequence to exercise
/// the free-list recycling path.  Each `worker()` allocates VARIABLES /
/// JUMP_ARGS / MAILBOX / ExecCtx / ProcessCtx via `rt_allocate`; on death the
/// scheduler calls `rt_free` on every fat-ptr.  If the freelist push/pop logic
/// is broken (e.g. corrupted chain pointer at payload[0]) the second batch
/// reads garbage from a recycled chunk and either traps or prints wrong data.
///
/// 50 sequential spawns is enough to force at least one allocation to come
/// from the freelist (not the bump path) for every size class used by spawn.
#[test]
fn allocator_recycles_via_freelist() {
    let src = r#"
        @rt(rt_write)
        worker is { _ -> { rt_write(1 "x" 1) } }
        spawner is {
          n i64 -> {
            when 0 == n {
              true  -> { }
              false -> { worker() self(n-1) }
            }
          }
        }
        main is { _ -> { spawner(50) } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    let s = out.stdout_str();
    assert_eq!(
        s.matches('x').count(),
        50,
        "expected 50 worker prints, got {s:?}"
    );
}

/// Dynamic queue regression: spawn more actors than the static 256-slot
/// process_arr cap to confirm the scheduler's queue grows on demand.
///
/// Currently ignored — the queue is statically sized at 256 slots and writing
/// past the end silently corrupts memory.  Will be un-ignored once tasks
/// #38-#41 land (PROCESS_ARR_CAP field + grow on append).
#[test]
#[ignore = "needs dynamic queue grow (#38-#41)"]
fn scheduler_queue_grows_past_static_cap() {
    let src = r#"
        @rt(rt_write)
        worker is { _ -> { rt_write(1 "y" 1) } }
        spawner is {
          n i64 -> {
            when 0 == n {
              true  -> { }
              false -> { worker() self(n-1) }
            }
          }
        }
        main is { _ -> { spawner(300) } }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    let s = out.stdout_str();
    assert_eq!(s.matches('y').count(), 300);
}
