#==============================================================================
# runtime/rt_macos_x86_64.s — zr 语言运行时（macOS x86-64, Intel syntax）
#
# macOS 没有"纯 syscall + 自带启动代码"的惯例：ld64 生成的启动桩会调用
# `_main`，libSystem 负责进程初始化。因此本运行时做最薄的一层封装：
#   zr_write    -> _write
#   zr_read     -> _read
#   zr_seek     -> _lseek
#   zr_fstat_size -> _fstat$INODE64
#   zr_isatty   -> _isatty
#   zr_exit     -> _exit
#   zr_alloc    -> _malloc
#   zr_dealloc  -> _free
#   zr_realloc  -> _realloc
#   zr_spawn / zr_run_tasks -> 单线程任务队列
#
# System V AMD64 ABI：参数 rdi/rsi/rdx/rcx/r8/r9，返回值 rax；
# 调用前 rsp 必须 16 字节对齐。
#
# 栈对齐说明：
#   Darwin 的程序入口是被 dyld 用 `call` 调进来的（入口处 rsp ≡ 8 mod 16），
#   而 zr 编译器生成的 `_start` 直接 `call main`，没有再调整栈，于是
#   zr 代码发起的每一层调用都比 ABI 要求少 8 字节。libc 里用 `movaps`
#   落栈的函数（open、malloc 等）在这种栈上会直接段错误。
#   所以凡是调用 libc 的封装在调用前都用 `and rsp, -16` 把栈强制对齐
#   （rbp 保存调用者的栈指针，返回时用 rbp 还原）：对齐与不对齐的
#   调用方都能正常工作。编译器侧修好 `_start` 之后这段依旧是无害的。
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
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    mov     rdi, 2              // fd = stderr（rsi=msg, rdx=len 已由调用方设置）
    call    _write
    mov     rdi, 99             // panic exit code
    call    _exit
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#------------------------------------------------------------------------------
    .globl _zr_write
_zr_write:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _write
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#------------------------------------------------------------------------------
    .globl _zr_read
_zr_read:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _read
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_open(path, mode) -> isize
#   rdi = path (NUL 结尾), rsi = mode（0=只读 1=写 2=追加）
#
# macOS 的 O_* 标志与 Linux 不同（O_CREAT = 0x200），这里统一成调用方的
# 0/1/2 语义：只读、写（截断）、追加。
#------------------------------------------------------------------------------
    .globl _zr_open
_zr_open:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    mov     r8, rsi           # r8 = mode（注意：本文件是 Intel 语法，目标是 r8）
    cmp     r8, 0
    je      2f                # 只读
    cmp     r8, 1
    je      1f                # 写（截断）
    # 追加：O_WRONLY | O_CREAT | O_APPEND = 0x1 | 0x200 | 0x8
    mov     esi, 0x209
    jmp     3f
1:
    # 写（截断）：O_WRONLY | O_CREAT | O_TRUNC = 0x1 | 0x200 | 0x400
    mov     esi, 0x601
    jmp     3f
2:
    # 只读：O_RDONLY
    xor     esi, esi
3:
    mov     rdx, 0644         # mode
    call    _open
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_close(fd) -> i32
#   rdi = fd
#------------------------------------------------------------------------------
    .globl _zr_close
_zr_close:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _close
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_seek(fd, offset, whence) -> i64
#   rdi = fd, rsi = offset（字节数，可为负）, rdx = whence
#   whence: 0 = 起点(SEEK_SET), 1 = 当前位置(SEEK_CUR), 2 = 末尾(SEEK_END)
# lseek 的参数顺序与 C 一致，SEEK_SET/CUR/END 也正好是 0/1/2；
# 出错时 libc 返回 -1，正是我们对外的约定。
#------------------------------------------------------------------------------
    .globl _zr_seek
_zr_seek:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _lseek
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_fstat_size(fd) -> i64
#   rdi = fd
# macOS 的 struct stat 是 144 字节，st_size 在偏移 96（8 字节）。
# 注意：x86-64 上 libc 的 `fstat` 符号是 32 位 inode 的老版本（恒失败），
# 真正可用的是 `fstat$INODE64`（C 头文件里的 __DARWIN_INODE64(fstat)）。
# 返回文件大小；出错返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_fstat_size
_zr_fstat_size:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    sub     rsp, 144           // 144 字节的 stat 缓冲区
    mov     rsi, rsp           // statbuf
    call    _fstat$INODE64
    test    rax, rax
    jnz     .fstat_failed      # fstat 成功返回 0
    mov     rax, qword ptr [rsp+96]    // st_size
    jmp     .fstat_done
.fstat_failed:
    mov     rax, -1
.fstat_done:
    add     rsp, 144
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_isatty(fd) -> i32
#   rdi = fd
# libc 的 isatty：终端返回非 0，其它返回 0；这里统一成 1 / 0。
#------------------------------------------------------------------------------
    .globl _zr_isatty
_zr_isatty:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _isatty
    test    rax, rax
    setne   al
    movzx   rax, al
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#------------------------------------------------------------------------------
    .globl _zr_exit
_zr_exit:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _exit
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#   rdi = size, rsi = align（malloc 只用 size）
#------------------------------------------------------------------------------
    .globl _zr_alloc
_zr_alloc:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _malloc
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
#------------------------------------------------------------------------------
    .globl _zr_dealloc
_zr_dealloc:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    call    _free
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   rdi = ptr, rsi = old, rdx = new, rcx = align
#------------------------------------------------------------------------------
    .globl _zr_realloc
_zr_realloc:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16          // 调用点要求 rsp ≡ 0 (mod 16)
    mov     rsi, rdx            // realloc(ptr, new_size)：只看第 2 个参数
    call    _realloc
    mov     rsp, rbp
    pop     rbp
    ret
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
