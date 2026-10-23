#==============================================================================
# runtime/rt_windows_arm64.s — zr 语言运行时（Windows aarch64, AT&T syntax）
#
# 与 x86-64 版一一对应：kernel32 薄封装。
#
#   zr_write    -> WriteFile
#   zr_read     -> ReadFile
#   zr_seek     -> SetFilePointerEx
#   zr_fstat_size -> GetFileSizeEx
#   zr_isatty   -> GetConsoleMode
#   zr_exit     -> ExitProcess
#   zr_panic    -> WriteFile(stderr) + ExitProcess(99)
#   zr_alloc    -> HeapAlloc(GetProcessHeap())
#   zr_dealloc  -> HeapFree
#   zr_realloc  -> HeapRealloc
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
# zr_seek(fd, offset, whence) -> i64
#   x0=fd, x1=offset（字节数，可为负）, x2=whence
#   whence: 0 = 起点(FILE_BEGIN), 1 = 当前位置(FILE_CURRENT), 2 = 末尾(FILE_END)
# 用 SetFilePointerEx：x0=hFile, x1=移动距离（按值）, x2=&新偏移,
# x3=移动方式。返回新的文件偏移；出错返回 -1。
#------------------------------------------------------------------------------
    .globl zr_seek
zr_seek:
    sub    sp, sp, #0x60
    str    x30, [sp, #0x48]     // 跨调用保存返回地址
    mov    x4, x1               // offset（i64，需要完整 64 位）
    mov    w5, w2               // whence（32 位）
    mov    x6, x0               // fd
    cmp    x6, #0
    b.eq   zr_seek_stdin
    cmp    x6, #1
    b.eq   zr_seek_stdout
    cmp    x6, #2
    b.eq   zr_seek_stderr
    b      zr_seek_call         // 其他值视为原生 HANDLE

zr_seek_stdin:
    mov    x0, #-10
    b      zr_seek_call

zr_seek_stdout:
    mov    x0, #-11
    b      zr_seek_call

zr_seek_stderr:
    mov    x0, #-12

zr_seek_call:
    mov    x1, x4               // liDistanceToMove（按值传递）
    add    x2, sp, #0x30        // lpNewFilePointer
    mov    w3, w5               // dwMoveMethod
    bl     SetFilePointerEx
    cbz    w0, zr_seek_fail
    ldr    x0, [sp, #0x30]
    ldr    x30, [sp, #0x48]
    add    sp, sp, #0x60
    ret

zr_seek_fail:
    mov    x0, #-1
    ldr    x30, [sp, #0x48]
    add    sp, sp, #0x60
    ret

#------------------------------------------------------------------------------
# zr_fstat_size(fd) -> i64
#   x0=fd
# 用 GetFileSizeEx：x0=hFile, x1=&文件大小（LARGE_INTEGER）。
# 返回文件大小；出错返回 -1。
#------------------------------------------------------------------------------
    .globl zr_fstat_size
zr_fstat_size:
    sub    sp, sp, #0x60
    str    x30, [sp, #0x48]
    mov    x6, x0
    cmp    x6, #0
    b.eq   zr_size_stdin
    cmp    x6, #1
    b.eq   zr_size_stdout
    cmp    x6, #2
    b.eq   zr_size_stderr
    b      zr_size_call         // 其他值视为原生 HANDLE

zr_size_stdin:
    mov    x0, #-10
    b      zr_size_call

zr_size_stdout:
    mov    x0, #-11
    b      zr_size_call

zr_size_stderr:
    mov    x0, #-12

zr_size_call:
    add    x1, sp, #0x30        // lpFileSize
    bl     GetFileSizeEx
    cbz    w0, zr_size_fail
    ldr    x0, [sp, #0x30]
    ldr    x30, [sp, #0x48]
    add    sp, sp, #0x60
    ret

zr_size_fail:
    mov    x0, #-1
    ldr    x30, [sp, #0x48]
    add    sp, sp, #0x60
    ret

#------------------------------------------------------------------------------
# zr_isatty(fd) -> i32
#   x0=fd
# 用 GetConsoleMode：成功说明句柄是控制台（终端），返回 1；否则返回 0。
#------------------------------------------------------------------------------
    .globl zr_isatty
zr_isatty:
    sub    sp, sp, #0x60
    str    x30, [sp, #0x48]
    mov    x6, x0
    cmp    x6, #0
    b.eq   zr_tty_stdin
    cmp    x6, #1
    b.eq   zr_tty_stdout
    cmp    x6, #2
    b.eq   zr_tty_stderr
    b      zr_tty_call          // 其他值视为原生 HANDLE

zr_tty_stdin:
    mov    x0, #-10
    b      zr_tty_call

zr_tty_stdout:
    mov    x0, #-11
    b      zr_tty_call

zr_tty_stderr:
    mov    x0, #-12

zr_tty_call:
    add    x1, sp, #0x30        // lpMode
    bl     GetConsoleMode
    cbz    w0, zr_tty_not_tty
    mov    x0, #1
    ldr    x30, [sp, #0x48]
    add    sp, sp, #0x60
    ret

zr_tty_not_tty:
    mov    x0, #0
    ldr    x30, [sp, #0x48]
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
#==============================================================================
# 文件系统 / 路径 / 时间原语：本平台暂未实现，统一返回 -1。
# 符号必须存在，否则 `use fs` 的程序链接失败。
#==============================================================================
    .text

    .globl zr_mkdir
zr_mkdir:
    mov     x0, #-1
    ret

    .globl zr_rmdir
zr_rmdir:
    mov     x0, #-1
    ret

    .globl zr_unlink
zr_unlink:
    mov     x0, #-1
    ret

    .globl zr_rename
zr_rename:
    mov     x0, #-1
    ret

    .globl zr_path_exists
zr_path_exists:
    mov     x0, #-1
    ret

    .globl zr_is_dir
zr_is_dir:
    mov     x0, #-1
    ret

    .globl zr_file_size
zr_file_size:
    mov     x0, #-1
    ret

    .globl zr_readdir_count
zr_readdir_count:
    mov     x0, #-1
    ret

    .globl zr_readdir_name
zr_readdir_name:
    mov     x0, #-1
    ret

    .globl zr_getcwd
zr_getcwd:
    mov     x0, #-1
    ret

    .globl zr_kill
zr_kill:
    mov     x0, #-1
    ret

    .globl zr_execvp
zr_execvp:
    mov     x0, #-1
    ret

    .globl zr_sleep_ms
zr_sleep_ms:
    mov     x0, #-1
    ret

    .globl zr_time_ns
zr_time_ns:
    mov     x0, #-1
    ret

    .globl zr_time_unix_ns
zr_time_unix_ns:
    mov     x0, #-1
    ret
