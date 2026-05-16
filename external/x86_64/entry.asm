    .intel_syntax noprefix
    .text

# ──────────────────────────────────────────────────────────────────────
# entry.asm — ELF entry-point shim for the lake-native runtime.
#
# Linux hands control to `_start` with the initial stack laid out per
# the x86-64 SysV ABI (excerpt of `man execve` / ELF spec):
#
#   [rsp]                  argc                       (int)
#   [rsp + 8]              argv[0]                    (char*)
#   [rsp + 16]             argv[1]
#   …
#   [rsp + 8*(argc+1)]     NULL                       (argv terminator)
#   [rsp + 8*(argc+2)]     envp[0]
#   …
#   NULL                                              (envp terminator)
#   ELF auxv …
#
# Cranelift cannot describe a calling convention that pulls args off
# the raw stack (see wasmtime#5996), so this shim runs before any
# Lake-generated code: it snapshots argc/argv/envp into BSS globals
# so Lake-side helpers can read them later, 16-byte-aligns rsp per
# SysV ABI, then enters `lake_main` (Cranelift-emitted scheduler).
#
# `lake_main` itself calls `rt_exit` and never returns — but we keep
# a defensive `SYS_exit` fallback after the `call` for robustness.
# ──────────────────────────────────────────────────────────────────────
    .global _start
    .type _start, @function
_start:
    mov rax, [rsp]                  # argc
    mov [rip + lake_argc], rax

    lea rbx, [rsp + 8]              # argv = &argv[0]
    mov [rip + lake_argv], rbx

    # envp = argv + (argc + 1) * 8
    inc rax
    shl rax, 3
    lea rcx, [rbx + rax]
    mov [rip + lake_envp], rcx

    and rsp, -16                    # 16-byte align (SysV ABI)

    call lake_main

    mov rax, 60                     # SYS_exit
    xor rdi, rdi
    syscall

# ──────────────────────────────────────────────────────────────────────
# rt_argc_raw()       -> i64   process argc
# rt_argv_raw()       -> i64   ptr to argv[0]
# rt_envp_raw()       -> i64   ptr to envp[0]
#
# Lake-side wrappers in `std/env.lake` convert these raw pointers into
# Lake `buf` values via `rt_cstr_len` + a copy loop.
# ──────────────────────────────────────────────────────────────────────
    .global rt_argc_raw
    .type rt_argc_raw, @function
rt_argc_raw:
    mov rax, [rip + lake_argc]
    ret

    .global rt_argv_raw
    .type rt_argv_raw, @function
rt_argv_raw:
    mov rax, [rip + lake_argv]
    ret

    .global rt_envp_raw
    .type rt_envp_raw, @function
rt_envp_raw:
    mov rax, [rip + lake_envp]
    ret

# ──────────────────────────────────────────────────────────────────────
# rt_cstr_len(ptr: *const u8) -> i64
#
# strlen for a null-terminated C string — byte-at-a-time, sufficient
# for argv / envp entries (typically short paths and var=value pairs).
# ──────────────────────────────────────────────────────────────────────
    .global rt_cstr_len
    .type rt_cstr_len, @function
rt_cstr_len:
    xor rax, rax
.Lrt_cstr_len_loop:
    cmp byte ptr [rdi + rax], 0
    je  .Lrt_cstr_len_done
    inc rax
    jmp .Lrt_cstr_len_loop
.Lrt_cstr_len_done:
    ret

# ──────────────────────────────────────────────────────────────────────
# BSS globals — populated once by `_start` before `lake_main` runs.
# Aligned to 8 bytes for ABI-correct loads via Cranelift `iconst64`.
# ──────────────────────────────────────────────────────────────────────
    .bss
    .global lake_argc
    .align 8
lake_argc: .skip 8
    .global lake_argv
    .align 8
lake_argv: .skip 8
    .global lake_envp
    .align 8
lake_envp: .skip 8
