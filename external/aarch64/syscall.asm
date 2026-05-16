// ──────────────────────────────────────────────────────────────────────
// aarch64/syscall.asm — direct Linux/aarch64 syscall trampoline.
//
// rt_syscall(n, a1, a2, a3, a4, a5, a6) -> i64
//
// Lake calls this with the SysV / AAPCS64 ABI: args in x0..x6.
// Linux aarch64 syscall ABI:
//   x8     = syscall number
//   x0..x5 = up to 6 syscall args
//   svc 0  = trap into kernel
//   x0     = return value (errno encoded as -errno)
//
// Mapping: the caller's first AAPCS64 arg (x0) holds the syscall
// number, then args[1..6] follow in x1..x6.  Shuffle x0..x6 → x8 + x0..x5.
// ──────────────────────────────────────────────────────────────────────
    .text
    .global rt_syscall
    .type rt_syscall, @function
rt_syscall:
    mov     x8, x0
    mov     x0, x1
    mov     x1, x2
    mov     x2, x3
    mov     x3, x4
    mov     x4, x5
    mov     x5, x6
    svc     #0
    ret
