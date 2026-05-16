.text
.global rt_syscall
.type rt_syscall, @function

rt_syscall:
    # аргументы по SysV ABI:
    # rdi = n
    # rsi = a1
    # rdx = a2
    # rcx = a3
    # r8  = a4
    # r9  = a5
    # [rsp+8] = a6

    mov %rdi, %rax      # syscall number -> rax
    mov %rsi, %rdi      # a1
    mov %rdx, %rsi      # a2
    mov %rcx, %rdx      # a3
    mov %r8,  %r10      # a4 (ВАЖНО: r10, не rcx)
    mov %r9,  %r8       # a5
    mov 8(%rsp), %r9   # a6

    syscall

    ret
