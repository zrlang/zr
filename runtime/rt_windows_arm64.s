#==============================================================================
# runtime/rt_windows_arm64.s — zr 语言运行时（Windows aarch64, AT&T syntax）
#
# 与 x86-64 版一一对应：kernel32 薄封装。
#
# Windows ARM64 ABI：
#   前 4 个参数位于 x0..x3，第 5 个起在栈上（[sp]）；
#   没有 shadow space，x18 为平台保留，x29/x30 用于帧与返回地址；
#   栈在公开接口边界保持 16 字节对齐。
#
# 跨调用保存的临时值一律放栈上（[sp+0x28] 起），不污染 x19..x28 等
# callee-save 寄存器。
#==============================================================================

    .text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   x0 = msg, x1 = len
# 写 stderr，退出码 99。
#------------------------------------------------------------------------------
    .globl zr_panic
zr_panic:
    sub    sp, sp, #0x40
    mov    x2, x1              // count = len
    mov    x1, x0              // buf = msg
    mov    x0, #-12            // STD_ERROR_HANDLE
    add    x3, sp, #0x20       // &written
    str    xzr, [sp]           // lpOverlapped = NULL
    bl     WriteFile
    mov    x0, #99             // panic exit code
    bl     ExitProcess
    add    sp, sp, #0x40
    brk    #0

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#   x0 = fd, x1 = buf, x2 = count
# 返回写入字节数；失败返回 -1。
#------------------------------------------------------------------------------
    .globl zr_write
zr_write:
    sub    sp, sp, #0x60
    str    x1, [sp, #0x28]     // buf
    str    x2, [sp, #0x30]     // count
    cmp    x0, #0
    b.eq   zr_write_stdin
    cmp    x0, #1
    b.eq   zr_write_stdout
    cmp    x0, #2
    b.eq   zr_write_stderr
    b      zr_write_have_handle        // 其他值视为原生 HANDLE

zr_write_stdout:
    mov    x0, #-11
    b      zr_write_have_handle

zr_write_stderr:
    mov    x0, #-12
    b      zr_write_have_handle

zr_write_stdin:
    mov    x0, #-10

zr_write_have_handle:
    ldr    x1, [sp, #0x28]
    ldr    x2, [sp, #0x30]
    add    x3, sp, #0x20       // &written
    str    xzr, [sp]           // lpOverlapped = NULL
    bl     WriteFile
    cbz    w0, zr_write_fail
    ldr    w0, [sp, #0x20]
    add    sp, sp, #0x60
    ret

zr_write_fail:
    mov    x0, #-1
    add    sp, sp, #0x60
    ret

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#   x0 = fd, x1 = buf, x2 = count
#------------------------------------------------------------------------------
    .globl zr_read
zr_read:
    sub    sp, sp, #0x60
    str    x1, [sp, #0x28]
    str    x2, [sp, #0x30]
    cmp    x0, #0
    b.eq   zr_read_stdin
    cmp    x0, #1
    b.eq   zr_read_stdout
    cmp    x0, #2
    b.eq   zr_read_stderr
    b      zr_read_have_handle

zr_read_stdout:
    mov    x0, #-11
    b      zr_read_have_handle

zr_read_stderr:
    mov    x0, #-12
    b      zr_read_have_handle

zr_read_stdin:
    mov    x0, #-10

zr_read_have_handle:
    ldr    x1, [sp, #0x28]
    ldr    x2, [sp, #0x30]
    add    x3, sp, #0x20
    str    xzr, [sp]
    bl     ReadFile
    cbz    w0, zr_read_fail
    ldr    w0, [sp, #0x20]
    add    sp, sp, #0x60
    ret

zr_read_fail:
    mov    x0, #-1
    add    sp, sp, #0x60
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#   x0 = code
#------------------------------------------------------------------------------
    .globl zr_exit
zr_exit:
    bl     ExitProcess
    brk    #0

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#   x0 = size, x1 = align（HeapAlloc 只用 size）
#------------------------------------------------------------------------------
    .globl zr_alloc
zr_alloc:
    sub    sp, sp, #0x40
    str    x0, [sp, #0x20]     // 跨调用保存 size
    bl     GetProcessHeap
    mov    x1, x0              // hHeap
    ldr    x0, [sp, #0x20]     // dwBytes
    mov    x2, #0              // dwFlags
    bl     HeapAlloc
    add    sp, sp, #0x40
    ret

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
#   x0 = ptr
#------------------------------------------------------------------------------
    .globl zr_dealloc
zr_dealloc:
    sub    sp, sp, #0x40
    str    x0, [sp, #0x20]
    bl     GetProcessHeap
    mov    x1, x0              // hHeap
    ldr    x0, [sp, #0x20]     // lpAddress
    mov    x2, #0              // dwFlags
    bl     HeapFree
    add    sp, sp, #0x40
    ret

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   x0 = ptr, x1 = old, x2 = new, x3 = align
#------------------------------------------------------------------------------
    .globl zr_realloc
zr_realloc:
    sub    sp, sp, #0x40
    str    x0, [sp, #0x20]     // ptr
    str    x2, [sp, #0x28]     // new size
    bl     GetProcessHeap
    mov    x1, x0              // hHeap
    ldr    x0, [sp, #0x20]
    mov    x2, #0              // dwFlags
    ldr    x3, [sp, #0x28]
    bl     HeapRealloc
    add    sp, sp, #0x40
    ret