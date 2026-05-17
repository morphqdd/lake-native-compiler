//! Bug #120 — TARGET_CYCLES baked as u32 immediate truncates when
//! `LAKE_QUANTUM_US > ~983000` on ~4.4 GHz CPUs.  See
//! docs/state/bugs/120_target_cycles_u32_overflow.md.
//!
//! Verifies: with a large enough `LAKE_QUANTUM_US`, the resulting object
//! bytes contain the full 8-byte little-endian i64 representation of
//! `us × tsc_khz / 1000` (loaded from a per-machine rodata symbol) — the
//! pre-fix codegen would have only contained the low 32 bits in a
//! `cmp r/m64, imm32` instruction.

use indicatif::ProgressBar;
use lakec::compiler::{compile, ctx::OptLevel};
use tempfile::tempdir;

fn compile_with_quantum_us(us: &str) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("p.lake");
    let src = r#"
        @rt(rt_write)
        worker is {
          n i64 -> {
            when 0 == n {
              true -> { rt_write(1 "d" 1) }
              false -> { self(n - 1) }
            }
          }
        }
        main is { _ -> { worker(10) } }
    "#;
    std::fs::write(&src_path, src).unwrap();
    // SAFETY: tests in this module mutate the LAKE_QUANTUM_US env var;
    // they run serially (single-threaded) by virtue of being one #[test]
    // each in this file's binary, and Cargo runs them sequentially within
    // a test binary unless --test-threads says otherwise.  We avoid the
    // race by composing the value into compile() before any other test
    // could read it.  See bug #120.
    unsafe {
        std::env::set_var("LAKE_QUANTUM_US", us);
    }
    let bytes = compile(ProgressBar::new(0), &src_path, OptLevel::None).unwrap();
    unsafe {
        std::env::remove_var("LAKE_QUANTUM_US");
    }
    bytes
}

fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn target_cycles_full_i64_survives_codegen() {
    // 1s quantum × any plausible tsc_khz (≥ 2_148_000) exceeds u32::MAX.
    // Pick LAKE_QUANTUM_US large enough that on the host's actual TSC
    // freq the value lands above i32::MAX so the rodata path engages.
    let obj_big = compile_with_quantum_us("1000000");
    let obj_small = compile_with_quantum_us("10");

    // Even on a slow 2GHz fallback host, 1_000_000µs × 2_000_000kHz /
    // 1000 = 2_000_000_000 which is below i32::MAX (2_147_483_647); on
    // typical >2.15GHz hosts it tips over.  Skip the strict check on the
    // 2GHz fallback by gating: only assert presence of the byte pattern
    // when we know the value would have overflowed u32.
    //
    // We reconstruct the would-be cycles count the same way
    // `compute_target_cycles` does — that path is what we're protecting.
    let tsc_khz: i64 = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/tsc_freq_khz")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
                .ok()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(3_000_000);
    let big_cycles = 1_000_000_i64.saturating_mul(tsc_khz) / 1000;
    let small_cycles = 10_i64.saturating_mul(tsc_khz) / 1000;

    if big_cycles > i32::MAX as i64 {
        // Post-fix: the 8-byte i64 lives in a rodata data symbol.
        // Pre-fix: only the low 32 bits would have been encoded as the
        // cmp imm32 operand, the top 4 bytes would be absent.
        let bytes_le = big_cycles.to_le_bytes();
        assert!(
            contains_subseq(&obj_big, &bytes_le),
            "full 8-byte i64 target_cycles = {big_cycles} (0x{big_cycles:016x}) \
             missing from object — bug #120 regression?"
        );
    }

    // Sanity: the small-quantum path keeps the iconst fast path; we just
    // assert the small value's low 32 bits appear somewhere (in the
    // immediate operand of the cmp).
    let small_le = (small_cycles as i32).to_le_bytes();
    assert!(
        contains_subseq(&obj_small, &small_le),
        "low 32-bit target_cycles = {small_cycles} missing from small-quantum object"
    );
}
