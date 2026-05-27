//! Smoke tests for #150 phase 2 — `rt_allocate_slab(class_idx)`.
//!
//! Verifies the rt-fn returns distinct addresses for back-to-back
//! allocations (sanity check that the bitmap bit-clear + chunk-index
//! arithmetic both advance).  Slab-crossing behaviour is covered by the
//! lib tests of `SlabLayout` + visual inspection here — driving
//! `chunks_per_slab(4) + 1` (= 256) calls from Lake would be a tedious
//! exit-code-based test; phase 5's RSS-plateau test is the real proof.

use super::common::run;

/// Two back-to-back `rt_allocate_slab(4)` calls return non-zero,
/// distinct addresses.  Exits 0 on success, non-zero on failure.
#[test]
fn slab_alloc_two_class_4_chunks_distinct() {
    let src = r#"
        @rt(rt_allocate_slab)
        @rt(rt_exit)

        main is {
          _ -> {
            let a = rt_allocate_slab(4)
            let b = rt_allocate_slab(4)
            // Distinct + non-zero.  `a != b` and `a != 0` and `b != 0`.
            when a == 0 { true -> { rt_exit(11) } _ -> {} }
            when b == 0 { true -> { rt_exit(12) } _ -> {} }
            when a == b { true -> { rt_exit(13) } _ -> { rt_exit(0) } }
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}

/// Three back-to-back class-0 (16 B) allocations are all distinct +
/// non-zero.  Different chunk-size class exercises a different scan
/// path (class 0 has ~4062 chunks/slab, 64 bitmap words).
#[test]
fn slab_alloc_three_class_0_chunks_distinct() {
    let src = r#"
        @rt(rt_allocate_slab)
        @rt(rt_exit)

        main is {
          _ -> {
            let a = rt_allocate_slab(0)
            let b = rt_allocate_slab(0)
            let c = rt_allocate_slab(0)
            when a == 0 { true -> { rt_exit(21) } _ -> {} }
            when b == 0 { true -> { rt_exit(22) } _ -> {} }
            when c == 0 { true -> { rt_exit(23) } _ -> {} }
            when a == b { true -> { rt_exit(24) } _ -> {} }
            when a == c { true -> { rt_exit(25) } _ -> {} }
            when b == c { true -> { rt_exit(26) } _ -> { rt_exit(0) } }
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}

// ─── #150 phase 3 — `rt_free_slab` ─────────────────────────────────────

/// Reuse-after-free: alloc class 4, free it, alloc again — the bitmap
/// bit reset makes the second alloc's ctz scan pick the same slot.
#[test]
fn slab_free_reuse_after_free_class_4() {
    let src = r#"
        @rt(rt_allocate_slab)
        @rt(rt_free_slab)
        @rt(rt_exit)

        main is {
          _ -> {
            let a = rt_allocate_slab(4)
            when a == 0 { true -> { rt_exit(31) } _ -> {} }
            rt_free_slab(a)
            let b = rt_allocate_slab(4)
            when b == 0 { true -> { rt_exit(32) } _ -> {} }
            when a == b { true -> { rt_exit(0) } _ -> { rt_exit(33) } }
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}

/// Full-cycle reclaim: class 11 has 1 chunk per slab.  Allocate the only
/// chunk, free it (drops free_count back to chunks_per_slab → triggers
/// unlink + munmap), then allocate again.  Must succeed: the next alloc
/// walks the now-empty slabs_head, finds nothing, and mmaps a fresh slab.
/// We can't reliably check that the address differs (Linux often hands
/// back the same VMA), so this test verifies the round-trip doesn't
/// crash and the second alloc returns non-zero.
#[test]
fn slab_free_full_cycle_reclaim_class_11() {
    let src = r#"
        @rt(rt_allocate_slab)
        @rt(rt_free_slab)
        @rt(rt_exit)

        main is {
          _ -> {
            let a = rt_allocate_slab(11)
            when a == 0 { true -> { rt_exit(41) } _ -> {} }
            rt_free_slab(a)
            // slab munmapped; next alloc must mmap fresh.
            let b = rt_allocate_slab(11)
            when b == 0 { true -> { rt_exit(42) } _ -> { rt_exit(0) } }
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}

/// No double-reclaim: class 10 has 3 chunks per slab.  Allocate all
/// three (K = 3), free two of them (free_count goes from 0 → 2,
/// NOT yet 3), then allocate a fourth.  Should succeed at one of the
/// just-freed slots — slab must NOT have been munmapped (free_count
/// never reached K), so no crash on the bitmap scan of the existing
/// slab.
#[test]
fn slab_free_no_premature_reclaim_class_10() {
    let src = r#"
        @rt(rt_allocate_slab)
        @rt(rt_free_slab)
        @rt(rt_exit)

        main is {
          _ -> {
            let a = rt_allocate_slab(10)
            let b = rt_allocate_slab(10)
            let c = rt_allocate_slab(10)
            when a == 0 { true -> { rt_exit(51) } _ -> {} }
            when b == 0 { true -> { rt_exit(52) } _ -> {} }
            when c == 0 { true -> { rt_exit(53) } _ -> {} }
            // Free a and b but not c — free_count = 2 < 3.
            rt_free_slab(a)
            rt_free_slab(b)
            // Slab still alive (c is in use).  New alloc reuses one
            // of the freed slots.
            let d = rt_allocate_slab(10)
            when d == 0 { true -> { rt_exit(54) } _ -> {} }
            // d must reuse one of {a, b}; distinct from c (still live).
            when d == c { true -> { rt_exit(55) } _ -> {} }
            rt_exit(0)
          }
        }
    "#;
    let out = run(src).unwrap();
    assert_eq!(out.exit_code, 0, "stderr: {:?}", out.stderr);
}
