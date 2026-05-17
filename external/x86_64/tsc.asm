// x86_64/tsc.asm — rdtsc trampoline.
//
// rt_tsc_now() -> i64
// Reads the timestamp counter and returns it in rax (SysV ABI).
    .text
    .global rt_tsc_now
    .type rt_tsc_now, @function
rt_tsc_now:
    rdtsc
    shl     $32, %rdx
    or      %rdx, %rax
    ret
