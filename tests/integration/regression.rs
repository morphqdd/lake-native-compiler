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

/// Dynamic queue: spawn more actors than the initial 256-slot process_arr
/// capacity to confirm the scheduler's queue grows on demand (doubles each
/// time, copying the old payload via inline loop, freeing the old buffer).
#[test]
fn scheduler_queue_grows_past_initial_cap() {
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

/// Bug #73: two sequential calls to a self-loop ret-machine with a side
/// effect.  The first actor (cd_actor_1) ran correctly and produced its
/// reply; on death the scheduler freed its proc_ctx fat-ptr, which the
/// allocator pushed to a free list and immediately handed out to the
/// second actor (cd_actor_2).  Because pids were process_ctx addresses,
/// cd_actor_2 inherited cd_actor_1's pid — wait2's filter accepted a
/// stale identifier and main returned r2 = stale value without ever
/// running the second actor.  Fixed by switching to a monotonic pid +
/// pid_table indirection: dead actors leave a permanent gap in the
/// table, so a recycled proc_ctx address can never collide with an
/// older pid.
#[test]
fn sequential_ret_self_loop_actors_each_run_once() {
    let src = r#"
        @rt(rt_exit)
        @rt(rt_write)

        cd is {
          x i64 -> ret i64 {
            rt_write(1 "*" 1)
            when x == 0 {
              true -> { rt_write(1 "Z" 1) ret 99 }
              _    -> { self(x - 1) }
            }
          }
        }

        main is {
          _ -> {
            let r1 = cd(2)
            rt_write(1 "[" 1)
            let r2 = cd(3)
            rt_write(1 "]" 1)
            rt_exit(r1 + r2)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 198, "stderr: {:?}", out.stderr);
    let s = out.stdout_str();
    // cd(2) runs: 3 stars + Z. cd(3) runs: 4 stars + Z. main brackets `[`
    // and `]` between them.  Order constraints: brackets land between the
    // two actors (main parks until cd_actor_1 replies, then prints `[`,
    // spawns cd_actor_2, parks again, prints `]` after the second reply).
    assert_eq!(
        s.matches('*').count(),
        7,
        "expected 7 stars (3 from cd(2) + 4 from cd(3)), got {s:?}"
    );
    assert_eq!(
        s.matches('Z').count(),
        2,
        "expected 2 Z markers (one per actor), got {s:?}"
    );
    assert!(s.contains('['), "missing `[` separator: {s:?}");
    assert!(s.contains(']'), "missing `]` separator: {s:?}");
}
