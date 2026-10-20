#==============================================================================
# runtime/rt_macos_x86_64.s — zr 语言运行时（macOS x86-64, Intel syntax）
#
# macOS 没有"纯 syscall + 自带启动代码"的惯例：ld64 生成的启动桩会调用
# `_main`，libSystem 负责进程初始化。因此本运行时做最薄的一层封装：
#   zr_write    -> _write
#   zr_read     -> _read
#   zr_exit     -> _exit
#   zr_alloc    -> _malloc
#   zr_dealloc  -> _free
#   zr_realloc  -> _realloc
#   zr_spawn / zr_run_tasks -> 单线程任务队列
#
# System V AMD64 ABI：参数 rdi/rsi/rdx/rcx/r8/r9，返回值 rax；
# 调用前 rsp 必须 16 字节对齐（push rbp 恰好恢复对齐）。
#
# 符号名带前导下划线（Mach-O 对 C 符号的编码规则）。
#==============================================================================

    .intel_syntax noprefix

    .section __TEXT,__text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   rdi = msg, rsi = len
# 写 stderr(fd=2)，退出码 99。
#------------------------------------------------------------------------------
    .globl _zr_panic
_zr_panic:
    push    rbp
    mov     rbp, rsp
    mov     rdi, 2              // fd = stderr（rsi=msg, rdx=len 已由调用方设置）
    call    _write
    mov     rdi, 99             // panic exit code
    call    _exit
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#------------------------------------------------------------------------------
    .globl _zr_write
_zr_write:
    jmp     _write

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#------------------------------------------------------------------------------
    .globl _zr_read
_zr_read:
    jmp     _read

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#------------------------------------------------------------------------------
    .globl _zr_exit
_zr_exit:
    jmp     _exit

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#   rdi = size, rsi = align（malloc 只用 size）
#------------------------------------------------------------------------------
    .globl _zr_alloc
_zr_alloc:
    jmp     _malloc

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
#------------------------------------------------------------------------------
    .globl _zr_dealloc
_zr_dealloc:
    jmp     _free

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   rdi = ptr, rsi = old, rdx = new, rcx = align
#------------------------------------------------------------------------------
    .globl _zr_realloc
_zr_realloc:
    mov     rsi, rdx            // realloc(ptr, new_size)：只看第 2 个参数
    jmp     _realloc
#------------------------------------------------------------------------------
# 任务队列：单线程执行器。
#   zr_spawn(frame)      把 future 帧挂到队尾，节点 = {frame, next}。
#   zr_run_tasks()       轮询所有任务；Ready 后调用帧头 drop 并释放节点。
# 帧头布局由 async_lower 固定：state=0, result=8, poll=16, drop=24。
#------------------------------------------------------------------------------
    .section __DATA,__bss
    .align 3
_zr_task_head:
    .quad 0
_zr_task_tail:
    .quad 0

    .section __TEXT,__text

    .globl _zr_spawn
_zr_spawn:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    mov     r12, rdi            // frame
    mov     edi, 16
    mov     esi, 8
    call    _zr_alloc
    test    rax, rax
    jz      .spawn_done
    mov     r13, rax
    mov     [r13], r12
    mov     qword ptr [r13+8], 0
    mov     rax, [rip + _zr_task_tail]
    test    rax, rax
    jz      .spawn_empty
    mov     [rax+8], r13
    jmp     .spawn_set_tail
.spawn_empty:
    mov     [rip + _zr_task_head], r13
.spawn_set_tail:
    mov     [rip + _zr_task_tail], r13
.spawn_done:
    pop     r13
    pop     r12
    pop     rbp
    ret

    .globl _zr_run_tasks
_zr_run_tasks:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    push    r14
    sub     rsp, 8
.run_loop:
    mov     r12, [rip + _zr_task_head]
    test    r12, r12
    jz      .run_done
    mov     r13, [r12+8]
    mov     [rip + _zr_task_head], r13
    test    r13, r13
    jnz     .run_have_next
    mov     qword ptr [rip + _zr_task_tail], 0
.run_have_next:
    mov     r14, [r12]
    mov     rdi, r14
    mov     rax, [r14+16]
    call    rax
    test    rax, rax
    jz      .run_pending
    mov     rdi, r14
    mov     rax, [r14+24]
    call    rax
    mov     rdi, r12
    mov     esi, 16
    mov     edx, 8
    call    _zr_dealloc
    jmp     .run_loop
.run_pending:
    mov     qword ptr [r12+8], 0
    mov     rax, [rip + _zr_task_tail]
    test    rax, rax
    jz      .run_pending_empty
    mov     [rax+8], r12
    jmp     .run_pending_tail
.run_pending_empty:
    mov     [rip + _zr_task_head], r12
.run_pending_tail:
    mov     [rip + _zr_task_tail], r12
    jmp     .run_loop
.run_done:
    add     rsp, 8
    pop     r14
    pop     r13
    pop     r12
    pop     rbp
    ret
