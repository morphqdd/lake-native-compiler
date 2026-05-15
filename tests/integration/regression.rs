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

/// #103: count_branch_vars sized the VARIABLES buffer with
/// `max(arm slot counts)` across when-arms, but the backend allocates
/// slots sequentially through both arms.  When the dead arm holds 2+
/// sequential ret-calls (each adds pid_let + sender + value slots
/// through Phase 2 lowering) and the live arm holds another ret-call,
/// the live arm's slot indices overran the buffer.  The overrun
/// stomped on adjacent scheduler memory; downstream the caller saw a
/// stale i64 in place of its expected buf and SIGSEGVed when
/// dereferencing it.
///
/// Surface symptom in lake-house: `house build` printed an empty
/// entry path then crashed.  Required cross-module + when with a
/// dead-arm holding 2+ ret-calls.
///
/// Fixed by treating when-arm slot counts as a sum, since arms are
/// emitted with non-overlapping slot indices.
#[test]
fn issue_103_when_dead_arm_slot_overrun() {
    // Self-loop scan_entry returns a positive value, so the false arm
    // runs (alloc + copy + trim).  The true arm holds 2 println calls
    // — dead at runtime but exercising the slot-overrun path.
    let src = r#"
        +std.io.{ println print_buf print }
        +std.bytes.{ size at trim }
        +std.process.{ alloc_or_die }
        @rt(rt_store rt_copy_bytes)

        const MAX_MANIFEST = 4096

        scan_entry is {
          flen i64 i i64 -> ret i64 {
            when i >= flen {
              true  -> { ret 7 * MAX_MANIFEST + 3 }
              false -> { self(flen i + 1) }
            }
          }
        }

        parse_entry is {
          file buf flen i64 -> ret buf {
            let packed = scan_entry(flen 0)
            when packed < 0 {
              true -> {
                println("a")
                println("b")
                ret file
              }
              false -> {
                let q = packed / MAX_MANIFEST
                let n = packed - q * MAX_MANIFEST
                let out = alloc_or_die(n)
                rt_copy_bytes(out 0 file q n)
                let _t = trim(out n)
                ret out
              }
            }
          }
        }

        build_input is {
          _pad i64 -> ret buf {
            let f = alloc_or_die(11)
            rt_store(f 101 1 0) rt_store(f 110 1 1) rt_store(f 116 1 2)
            rt_store(f 114 1 3) rt_store(f 121 1 4) rt_store(f 32 1 5)
            rt_store(f 34 1 6)  rt_store(f 97 1 7)  rt_store(f 98 1 8)
            rt_store(f 99 1 9)  rt_store(f 34 1 10)
            let _t = trim(f 11)
            ret f
          }
        }

        main is {
          _ -> {
            let m = build_input(0)
            let entry = parse_entry(m size(m))
            let es = size(entry)
            when es {
              3 -> { print("OK: ") print_buf(entry) println("") }
              _ -> { println("BAD") }
            }
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    assert!(
        out.stdout_str().contains("OK: abc"),
        "expected `OK: abc`, got: {:?}",
        out.stdout_str()
    );
}

/// #103 corollary: dead-arm with many sequential ret-calls must not
/// shift the slot indices used by the live arm, even when there are
/// no nested ret-calls.  This is the same root cause as the previous
/// test but with a flat (no inner-when) live arm — pins the slot
/// math without dependence on the wait-handler descent in Phase 4.
#[test]
fn issue_103_when_dead_arm_three_sequential_ret_calls() {
    let src = r#"
        +std.io.{ println }
        @rt(rt_exit)

        main is {
          _ -> {
            when 1 < 0 {
              true -> {
                println("never_a")
                println("never_b")
                println("never_c")
              }
              false -> {
                println("live")
              }
            }
            rt_exit(0)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
    let s = out.stdout_str();
    assert!(s.contains("live"), "live arm did not run: {s:?}");
    assert!(!s.contains("never_"), "dead arm leaked output: {s:?}");
}
