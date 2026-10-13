#==============================================================================
# runtime/rt_linux_x86_64.s — zr 语言运行时（Linux x86-64, AT&T syntax）
#
# 用纯 syscall 实现 7 个 `zr_*` 符号，不依赖 libc。
# System V AMD64 ABI。
#
# 系统调用号（x86-64 Linux）：write=1, read=0, exit=60, brk=12
#
# 内存分配：用 brk(12) 实现的简单 bump allocator。
# 教学级实现：不归还单个块，单次分配失败返回 NULL。
#==============================================================================

    .global zr_panic
    .global zr_write
    .global zr_read
    .global zr_exit
    .global zr_alloc
    .global zr_dealloc
    .global zr_realloc

    .section .text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   rdi = msg, rsi = len
# 写 stderr(fd=2)，退出码 99。
#------------------------------------------------------------------------------
zr_panic:
    movq    $1, %rax        # SYS_write
    movq    $2, %rdi        # fd = stderr
    # rsi = msg, rdx = len  already set by caller
    syscall
    movq    $60, %rax       # SYS_exit
    movq    $99, %rdi       # panic exit code
    syscall
    ud2

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#   rdi=fd, rsi=buf, rdx=count
#------------------------------------------------------------------------------
zr_write:
    movq    $1, %rax        # SYS_write
    syscall
    ret

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#------------------------------------------------------------------------------
zr_read:
    xorq    %rax, %rax      # SYS_read = 0
    syscall
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#------------------------------------------------------------------------------
zr_exit:
    movq    $60, %rax       # SYS_exit
    # rdi = code
    syscall
    ud2

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#
# Bump allocator:
#   heap_cursor / heap_end 是 bss 中的两个 qword。
#   首次调用用 brk(0) 取当前 program break，对齐到 16 字节。
#   若当前段不足，brk(heap_end + max(size,64K)) 扩展。
#   成功返回对齐后的地址；失败返回 NULL(0)。
#------------------------------------------------------------------------------
    .bss
    .align  8
heap_cursor:
    .quad   0
heap_end:
    .quad   0

    .text

zr_alloc:
    pushq   %rbp
    movq    %rsp, %rbp

    # r8 = align_up(size, 16)
    movq    %rdi, %rax
    addq    $15, %rax
    andq    $-16, %rax
    movq    %rax, %r8

    # if heap_cursor == 0 -> init
    movq    heap_cursor(%rip), %rax
    testq   %rax, %rax
    jnz     .have_cursor

    # brk(0) -> rax = current program break
    movq    $12, %rax       # SYS_brk
    xorq    %rdi, %rdi
    syscall
    addq    $15, %rax
    andq    $-16, %rax
    movq    %rax, heap_cursor(%rip)
    movq    %rax, heap_end(%rip)

.have_cursor:
    # if cursor + r8 <= end, 直接分配
    movq    heap_cursor(%rip), %rax
    addq    %r8, %rax
    cmpq    heap_end(%rip), %rax
    jle     .fits

    # 需要扩展：new_end = heap_end + max(r8, 65536)，对齐 16
    movq    %r8, %rcx
    addq    $65536, %rcx
    addq    $15, %rcx
    andq    $-16, %rcx
    addq    heap_end(%rip), %rcx
    movq    %rcx, %rdi
    movq    $12, %rax       # SYS_brk
    syscall
    cmpq    %rdi, %rax
    jl      .alloc_fail
    movq    %rax, heap_end(%rip)

.fits:
    movq    heap_cursor(%rip), %rax
    addq    %r8, heap_cursor(%rip)
    popq    %rbp
    ret

.alloc_fail:
    xorq    %rax, %rax
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
# Bump allocator 不归还单块；此实现仅为接口完整性（无操作）。
#------------------------------------------------------------------------------
zr_dealloc:
    ret

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   rdi=ptr, rsi=old_size, rdx=new_size, rcx=align
# 若 new_size <= old_size：原地返回 ptr。
# 否则：分配新块、拷贝旧内容、返回新块（不释放旧块）。
#------------------------------------------------------------------------------
zr_realloc:
    cmpq    %rsi, %rdx
    jle     .realloc_inplace

    pushq   %rsi
    pushq   %rdi
    pushq   %rdx
    movq    %rdx, %rdi      # size = new_size
    movq    %rcx, %rsi      # align
    call    zr_alloc
    popq    %rdx
    popq    %rdi
    popq    %rsi
    # rax=new_ptr, rdi=old_ptr, rsi=old_size, rdx=new_size
    movq    %rsi, %rcx      # rcx = min(old, new)
    cmpq    %rdx, %rcx
    cmovgq  %rdx, %rcx
    movq    %rdi, %rsi      # src = old_ptr
    movq    %rax, %rdi      # dst = new_ptr
    testq   %rcx, %rcx
    jz      .realloc_done

.copy_loop:
    movb    (%rsi), %r8b
    movb    %r8b, (%rdi)
    incq    %rsi
    incq    %rdi
    decq    %rcx
    jnz     .copy_loop

.realloc_done:
    ret

.realloc_inplace:
    movq    %rdi, %rax
    ret
