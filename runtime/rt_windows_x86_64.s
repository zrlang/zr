#==============================================================================
# runtime/rt_windows_x86_64.s — zr 语言运行时（Windows x86-64, Intel syntax）
#
# Windows 没有 Linux 那种"纯 syscall"入口，进程与堆都由系统 DLL 提供，
# 因此运行时是 kernel32 的薄封装：
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
#   zr_spawn / zr_run_tasks -> 单线程任务队列
#
# Microsoft x64 ABI：
#   前 4 个参数位于 rcx/rdx/r8/r9，第 5 个起在栈上；
#   **调用者**必须预留 32 字节 shadow space（[rsp..rsp+0x1F]），
#   因此每次调用前都要 `sub rsp, 0x40`：前 0x20 字节是 shadow space，
#   `[rsp+0x20]` 是第 5 个参数槽（lpOverlapped = NULL），
#   `[rsp+0x28]` 是我们自用的"实际读写字节数"缓冲区。
#   （函数入口处 rsp ≡ 8 (mod 16)，调用点要求 rsp ≡ 0 (mod 16)，
#   故需要自用栈槽的新函数用 `sub rsp, 0x48`：0x20 shadow space +
#   0x08 对齐 + 0x20 自用空间。）
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
# zr_seek(fd, offset, whence) -> i64
#   rcx = fd, rdx = offset（字节数，可为负）, r8 = whence
#   whence: 0 = 起点(FILE_BEGIN), 1 = 当前位置(FILE_CURRENT), 2 = 末尾(FILE_END)
# 用 SetFilePointerEx：rcx=hFile, rdx=移动距离（按值）, r8=&新偏移,
# r9=移动方式。返回新的文件偏移；出错返回 -1。
#------------------------------------------------------------------------------
    .globl zr_seek
zr_seek:
    push    rbx                 # 保存调用者的 rbx（callee-save）
    mov     rbx, rsp            # rbx = 原始栈指针（用于返回时还原）
    and     rsp, -16            # 强制 16 字节对齐：调用点要求 rsp ≡ 0 (mod 16)
    sub     rsp, 0x50           # 0x20 shadow space + 0x10 第 5 参数槽 + 0x20 自用
    mov     r10, rdx            # offset（i64，需要完整 64 位）
    mov     r11d, r8d           # whence（32 位）
    mov     rax, rcx            # fd
    cmp     rax, 0
    je      zr_seek_stdin
    cmp     rax, 1
    je      zr_seek_stdout
    cmp     rax, 2
    je      zr_seek_stderr
    jmp     zr_seek_call        // 其他值视为原生 HANDLE

zr_seek_stdin:
    mov     rcx, -10
    jmp     zr_seek_call

zr_seek_stdout:
    mov     rcx, -11
    jmp     zr_seek_call

zr_seek_stderr:
    mov     rcx, -12

zr_seek_call:
    mov     rdx, r10            // liDistanceToMove（按值传递）
    lea     r8, [rsp+0x30]      // lpNewFilePointer
    mov     r9d, r11d           // dwMoveMethod
    call    SetFilePointerEx
    test    eax, eax
    jz      zr_seek_failed
    mov     rax, qword ptr [rsp+0x30]
    mov     rsp, rbx
    pop     rbx
    ret

zr_seek_failed:
    mov     rax, -1
    mov     rsp, rbx
    pop     rbx
    ret

#------------------------------------------------------------------------------
# zr_fstat_size(fd) -> i64
#   rcx = fd
# 用 GetFileSizeEx：rcx=hFile, rdx=&文件大小（LARGE_INTEGER）。
# 返回文件大小；出错返回 -1。
#------------------------------------------------------------------------------
    .globl zr_fstat_size
zr_fstat_size:
    push    rbx
    mov     rbx, rsp
    and     rsp, -16            # 强制 16 字节对齐
    sub     rsp, 0x50           # 0x20 shadow space + 0x10 第 5 参数槽 + 0x20 自用
    mov     rax, rcx
    cmp     rax, 0
    je      zr_size_stdin
    cmp     rax, 1
    je      zr_size_stdout
    cmp     rax, 2
    je      zr_size_stderr
    jmp     zr_size_call        // 其他值视为原生 HANDLE

zr_size_stdin:
    mov     rcx, -10
    jmp     zr_size_call

zr_size_stdout:
    mov     rcx, -11
    jmp     zr_size_call

zr_size_stderr:
    mov     rcx, -12

zr_size_call:
    lea     rdx, [rsp+0x30]      // lpFileSize
    call    GetFileSizeEx
    test    eax, eax
    jz      zr_size_failed
    mov     rax, qword ptr [rsp+0x30]
    mov     rsp, rbx
    pop     rbx
    ret

zr_size_failed:
    mov     rax, -1
    mov     rsp, rbx
    pop     rbx
    ret

#------------------------------------------------------------------------------
# zr_isatty(fd) -> i32
#   rcx = fd
# 用 GetConsoleMode：成功说明句柄是控制台（终端），返回 1；否则返回 0。
#------------------------------------------------------------------------------
    .globl zr_isatty
zr_isatty:
    push    rbx
    mov     rbx, rsp
    and     rsp, -16            # 强制 16 字节对齐
    sub     rsp, 0x50           # 0x20 shadow space + 0x10 第 5 参数槽 + 0x20 自用
    mov     rax, rcx
    cmp     rax, 0
    je      zr_tty_stdin
    cmp     rax, 1
    je      zr_tty_stdout
    cmp     rax, 2
    je      zr_tty_stderr
    jmp     zr_tty_call         // 其他值视为原生 HANDLE

zr_tty_stdin:
    mov     rcx, -10
    jmp     zr_tty_call

zr_tty_stdout:
    mov     rcx, -11
    jmp     zr_tty_call

zr_tty_stderr:
    mov     rcx, -12

zr_tty_call:
    lea     rdx, [rsp+0x30]      // lpMode
    call    GetConsoleMode
    test    eax, eax
    jz      zr_tty_not_tty
    mov     eax, 1
    mov     rsp, rbx
    pop     rbx
    ret

zr_tty_not_tty:
    xor     eax, eax
    mov     rsp, rbx
    pop     rbx
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
#------------------------------------------------------------------------------
# 任务队列：单线程执行器。
#   zr_spawn(frame)      把 future 帧挂到队尾，节点 = {frame, next}。
#   zr_run_tasks()       轮询所有任务；Ready 后调用帧头 drop 并释放节点。
# 帧头布局由 async_lower 固定：state=0, result=8, poll=16, drop=24。
#------------------------------------------------------------------------------
    .section .bss
    .align 8
zr_task_head:
    .quad 0
zr_task_tail:
    .quad 0

    .text

    .globl zr_spawn
zr_spawn:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    sub     rsp, 0x20
    mov     r12, rcx            // frame
    mov     ecx, 16
    mov     edx, 8
    call    zr_alloc
    test    rax, rax
    jz      .spawn_done
    mov     r13, rax
    mov     [r13], r12
    mov     qword ptr [r13+8], 0
    mov     rax, [zr_task_tail]
    test    rax, rax
    jz      .spawn_empty
    mov     [rax+8], r13
    jmp     .spawn_set_tail
.spawn_empty:
    mov     [zr_task_head], r13
.spawn_set_tail:
    mov     [zr_task_tail], r13
.spawn_done:
    add     rsp, 0x20
    pop     r13
    pop     r12
    pop     rbp
    ret

    .globl zr_run_tasks
zr_run_tasks:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    push    r14
    sub     rsp, 0x28
.run_loop:
    mov     r12, [zr_task_head]
    test    r12, r12
    jz      .run_done
    mov     r13, [r12+8]
    mov     [zr_task_head], r13
    test    r13, r13
    jnz     .run_have_next
    mov     qword ptr [zr_task_tail], 0
.run_have_next:
    mov     r14, [r12]
    mov     rcx, r14
    mov     rax, [r14+16]
    call    rax
    test    rax, rax
    jz      .run_pending
    mov     rcx, r14
    mov     rax, [r14+24]
    call    rax
    mov     rcx, r12
    mov     edx, 16
    mov     r8d, 8
    call    zr_dealloc
    jmp     .run_loop
.run_pending:
    mov     qword ptr [r12+8], 0
    mov     rax, [zr_task_tail]
    test    rax, rax
    jz      .run_pending_empty
    mov     [rax+8], r12
    jmp     .run_pending_tail
.run_pending_empty:
    mov     [zr_task_head], r12
.run_pending_tail:
    mov     [zr_task_tail], r12
    jmp     .run_loop
.run_done:
    add     rsp, 0x28
    pop     r14
    pop     r13
    pop     r12
    pop     rbp
    ret
