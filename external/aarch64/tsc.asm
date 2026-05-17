// aarch64/tsc.asm — cntvct_el0 read trampoline.
//
// rt_tsc_now() -> i64
// Reads the virtual count register (monotonic, EL0-readable).
    .text
    .global rt_tsc_now
    .type rt_tsc_now, @function
rt_tsc_now:
    mrs     x0, cntvct_el0
    ret
