#==============================================================================
# runtime/rt_windows_x86_64.s — zr 语言运行时（Windows x86-64, Intel syntax）
#
# Windows 没有 Linux 那种"纯 syscall"入口，进程与堆都由系统 DLL 提供，
# 因此运行时是 kernel32 的薄封装：
#   zr_write    -> WriteFile
#   zr_read     -> ReadFile
#   zr_exit     -> ExitProcess
#   zr_panic    -> WriteFile(stderr) + ExitProcess(99)
#   zr_alloc    -> HeapAlloc(GetProcessHeap())
#   zr_dealloc  -> HeapFree
#   zr_realloc  -> HeapRealloc
#
# Microsoft x64 ABI：
#   前 4 个参数位于 rcx/rdx/r8/r9，第 5 个起在栈上；
#   **调用者**必须预留 32 字节 shadow space（[rsp..rsp+0x1F]），
#   因此每次调用前都要 `sub rsp, 0x40`：前 0x20 字节是 shadow space，
#   `[rsp+0x20]` 是第 5 个参数槽（lpOverlapped = NULL），
#   `[rsp+0x28]` 是我们自用的"实际读写字节数"缓冲区。
#
# 语法：MSVC / clang / mingw 都可以用 Intel 语法汇编，故显式声明。
# 链接需要 kernel32 的导入库（clang 驱动会自动附加）。
#==============================================================================

    .intel_syntax noprefix

    .text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   rcx = msg, rdx = len
# 写 stderr，退出码 99。
#------------------------------------------------------------------------------
    .globl zr_panic
zr_panic:
    sub     rsp, 0x40
    mov     r8, rdx             // count = len
    mov     rdx, rcx            // buf = msg
    mov     rcx, -12            // STD_ERROR_HANDLE
    lea     r9, [rsp+0x28]      // &written
    mov     qword ptr [rsp+0x20], 0 // lpOverlapped = NULL
    call    WriteFile
    mov     rcx, 99             // panic exit code
    call    ExitProcess
    add     rsp, 0x40
    ud2

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#   rcx = fd, rdx = buf, r8 = count
# 返回写入字节数；失败返回 -1。
#------------------------------------------------------------------------------
    .globl zr_write
zr_write:
    sub     rsp, 0x40
    mov     rax, rcx            // 保存 fd
    mov     r10, rdx            // buf
    mov     r11, r8             // count
    cmp     rax, 0
    je      zr_write_stdin
    cmp     rax, 1
    je      zr_write_stdout
    cmp     rax, 2
    je      zr_write_stderr
    jmp     zr_write_call       // 其他值视为原生 HANDLE

zr_write_stdout:
    mov     rcx, -11            // STD_OUTPUT_HANDLE
    jmp     zr_write_call

zr_write_stderr:
    mov     rcx, -12
    jmp     zr_write_call

zr_write_stdin:
    mov     rcx, -10

zr_write_call:
    mov     rdx, r10
    mov     r8, r11
    lea     r9, [rsp+0x28]
    mov     qword ptr [rsp+0x20], 0
    call    WriteFile
    test    eax, eax
    jz      zr_write_failed
    mov     eax, [rsp+0x28]
    add     rsp, 0x40
    ret

zr_write_failed:
    mov     rax, -1
    add     rsp, 0x40
    ret

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#   rcx = fd, rdx = buf, r8 = count
#------------------------------------------------------------------------------
    .globl zr_read
zr_read:
    sub     rsp, 0x40
    mov     rax, rcx
    mov     r10, rdx
    mov     r11, r8
    cmp     rax, 0
    je      zr_read_stdin
    cmp     rax, 1
    je      zr_read_stdout
    cmp     rax, 2
    je      zr_read_stderr
    jmp     zr_read_call

zr_read_stdout:
    mov     rcx, -11
    jmp     zr_read_call

zr_read_stderr:
    mov     rcx, -12
    jmp     zr_read_call

zr_read_stdin:
    mov     rcx, -10

zr_read_call:
    mov     rdx, r10
    mov     r8, r11
    lea     r9, [rsp+0x28]
    mov     qword ptr [rsp+0x20], 0
    call    ReadFile
    test    eax, eax
    jz      zr_read_failed
    mov     eax, [rsp+0x28]
    add     rsp, 0x40
    ret

zr_read_failed:
    mov     rax, -1
    add     rsp, 0x40
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#   rcx = code
#------------------------------------------------------------------------------
    .globl zr_exit
zr_exit:
    sub     rsp, 0x20
    call    ExitProcess
    add     rsp, 0x20
    ud2

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#   rcx = size, rdx = align（HeapAlloc 只用 size）
#------------------------------------------------------------------------------
    .globl zr_alloc
zr_alloc:
    sub     rsp, 0x40
    mov     [rsp+0x20], rcx     // 跨调用保存 size
    call    GetProcessHeap
    mov     rcx, [rsp+0x20]
    mov     rdx, rax
    xor     r8d, r8d            // dwFlags = 0
    call    HeapAlloc
    add     rsp, 0x40
    ret

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
#   rcx = ptr
#------------------------------------------------------------------------------
    .globl zr_dealloc
zr_dealloc:
    sub     rsp, 0x40
    mov     [rsp+0x20], rcx
    call    GetProcessHeap
    mov     rcx, [rsp+0x20]
    mov     rdx, rax
    xor     r8d, r8d            // dwFlags = 0
    call    HeapFree
    add     rsp, 0x40
    ret

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   rcx = ptr, rdx = old, r8 = new, r9 = align
#------------------------------------------------------------------------------
    .globl zr_realloc
zr_realloc:
    sub     rsp, 0x60
    mov     [rsp+0x20], rcx     // ptr
    mov     [rsp+0x28], r8      // new size
    call    GetProcessHeap
    mov     rcx, [rsp+0x20]
    mov     r8, [rsp+0x28]
    mov     rdx, rax
    xor     r9d, r9d            // dwFlags = 0
    call    HeapRealloc
    add     rsp, 0x60
    ret