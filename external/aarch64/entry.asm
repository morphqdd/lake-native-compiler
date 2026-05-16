// ──────────────────────────────────────────────────────────────────────
// aarch64/entry.asm — ELF entry-point shim for the lake-native runtime
// on Linux/aarch64.
//
// Linux hands control to `_start` with the initial stack laid out per
// the AArch64 PCS / Linux ABI:
//
//   [sp]                   argc                       (long)
//   [sp + 8]               argv[0]                    (char*)
//   [sp + 16]              argv[1]
//   …
//   [sp + 8*(argc+1)]      NULL                       (argv terminator)
//   [sp + 8*(argc+2)]      envp[0]
//   …
//   NULL                                              (envp terminator)
//   ELF auxv …
//
// Cranelift cannot describe the no-args entry calling convention, so
// this shim runs first: snapshot argc/argv/envp into BSS globals, then
// call lake_main (Cranelift-emitted scheduler).  lake_main calls
// rt_exit and never returns; the SVC fallback exists for robustness.
//
// AArch64 Linux syscall ABI (uapi):
//   x8     = syscall number
//   x0..x5 = args
//   svc 0  = trap
//   x0     = return value
// ──────────────────────────────────────────────────────────────────────

    .text
    .global _start
    .type _start, @function
_start:
    // x0 = argc
    ldr     x0, [sp]
    adrp    x1, lake_argc
    add     x1, x1, :lo12:lake_argc
    str     x0, [x1]

    // x1 = argv = sp + 8
    add     x2, sp, #8
    adrp    x3, lake_argv
    add     x3, x3, :lo12:lake_argv
    str     x2, [x3]

    // envp = argv + (argc + 1) * 8
    add     x0, x0, #1
    lsl     x0, x0, #3
    add     x4, x2, x0
    adrp    x5, lake_envp
    add     x5, x5, :lo12:lake_envp
    str     x4, [x5]

    // 16-byte align sp (already aligned per ABI, defensive).
    // AArch64 disallows `and sp, ...` directly — round-trip via x9.
    mov     x9, sp
    and     x9, x9, #-16
    mov     sp, x9

    bl      lake_main

    // SYS_exit fallback (aarch64: nr=93)
    mov     x8, #93
    mov     x0, #0
    svc     #0

// ──────────────────────────────────────────────────────────────────────
// rt_argc_raw / rt_argv_raw / rt_envp_raw — readers for the BSS globals
// populated by _start.  Mirror the x86_64 shim's interface.
// ──────────────────────────────────────────────────────────────────────
    .global rt_argc_raw
    .type rt_argc_raw, @function
rt_argc_raw:
    adrp    x0, lake_argc
    ldr     x0, [x0, :lo12:lake_argc]
    ret

    .global rt_argv_raw
    .type rt_argv_raw, @function
rt_argv_raw:
    adrp    x0, lake_argv
    ldr     x0, [x0, :lo12:lake_argv]
    ret

    .global rt_envp_raw
    .type rt_envp_raw, @function
rt_envp_raw:
    adrp    x0, lake_envp
    ldr     x0, [x0, :lo12:lake_envp]
    ret

// ──────────────────────────────────────────────────────────────────────
// rt_cstr_len(ptr: *const u8) -> i64
// Byte-at-a-time strlen for argv / envp entries.
// ──────────────────────────────────────────────────────────────────────
    .global rt_cstr_len
    .type rt_cstr_len, @function
rt_cstr_len:
    mov     x1, #0
.Lcstr_len_loop:
    ldrb    w2, [x0, x1]
    cbz     w2, .Lcstr_len_done
    add     x1, x1, #1
    b       .Lcstr_len_loop
.Lcstr_len_done:
    mov     x0, x1
    ret

// ──────────────────────────────────────────────────────────────────────
// BSS globals — populated once by `_start` before `lake_main` runs.
// 8-byte aligned for ABI-correct loads.
// ──────────────────────────────────────────────────────────────────────
    .bss
    .global lake_argc
    .align 3
lake_argc: .skip 8
    .global lake_argv
    .align 3
lake_argv: .skip 8
    .global lake_envp
    .align 3
lake_envp: .skip 8
