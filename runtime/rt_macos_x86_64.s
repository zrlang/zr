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

    .globl _zr_thread_create
    .globl _zr_thread_join
    .globl _zr_thread_self
    .globl _zr_process_fork
    .globl _zr_process_exec
    .globl _zr_process_wait
    .globl _zr_process_getpid

    .globl _zr_mkdir
    .globl _zr_rmdir
    .globl _zr_unlink
    .globl _zr_rename
    .globl _zr_path_exists
    .globl _zr_is_dir
    .globl _zr_file_size
    .globl _zr_readdir_count
    .globl _zr_readdir_name
    .globl _zr_getcwd
    .globl _zr_kill
    .globl _zr_execvp
    .globl _zr_sleep_ms
    .globl _zr_time_ns
    .globl _zr_time_unix_ns

#==============================================================================
# 函数指针表：用于间接调用外部函数，避免符号存根问题
#------------------------------------------------------------------------------
    .section __DATA,__data
    .align 3
_getpid_ptr:
    .quad _getpid
# 线程 trampoline 的地址（`lea reg, [rip+sym]` 在 Mach-O 汇编器里不可用，
# 改成从数据段取地址再间接调用）。
_zr_thread_trampoline_ptr:
    .quad _zr_thread_trampoline

    .section __TEXT,__text

#==============================================================================
# 线程支持（macOS 上暂存根，返回 ENOSYS）
//------------------------------------------------------------------------------
# zr_thread_create(start_routine, arg) -> i32
#   rdi = start_routine, rsi = arg
# 返回 -1 (ENOSYS)，macOS 上暂未实现真 pthread
//------------------------------------------------------------------------------
    .globl _zr_thread_create
_zr_thread_create:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    push    r14
    push    r15
    # 序言结束时 rsp ≡ 0 (mod 16)：入口 rsp ≡ 8，push rbp 变 0，再压 4 个
    # 寄存器（32 字节）仍是 0。System V AMD64 ABI 要求"call 之前 rsp ≡ 0"。

    mov     r12, rdi          // start_routine（zr 函数指针）
    mov     r13, rsi          // arg

    // 分配 pthread_t 槽（8 字节）
    mov     edi, 8
    mov     esi, 8
    call    _zr_alloc
    test    rax, rax
    jz      .thread_create_fail
    mov     r14, rax          // pthread_t*

    // 分配 trampoline 参数 { func_ptr, arg }（16 字节）
    mov     edi, 16
    mov     esi, 16
    call    _zr_alloc
    test    rax, rax
    jz      .thread_create_fail_trampoline
    mov     r15, rax

    mov     [r15], r12        // func_ptr
    mov     [r15 + 8], r13    // arg

    // pthread_create(pthread_t*, attr, start_routine, arg)
    // 参数: rdi = pthread_t*, rsi = attr, rdx = start_routine, rcx = arg
    mov     rdi, r14
    xor     esi, esi          // attr = NULL
    // trampoline 的地址从数据段里取：Mach-O 的汇编器不支持
    // `lea reg, [rip+sym]` 这种 RIP 相对寻址的符号形式。
    mov     rdx, qword ptr [_zr_thread_trampoline_ptr + rip]
    mov     rcx, r15
    call    _pthread_create
    test    rax, rax
    jz      .thread_create_success

    // 失败：清理
    mov     rdi, r15
    mov     esi, 16
    mov     edx, 16
    call    _zr_dealloc
    mov     rdi, r14
    mov     esi, 8
    mov     edx, 8
    call    _zr_dealloc
    xor     eax, eax          // 返回 NULL（失败）
    jmp     .thread_create_done

.thread_create_success:
    mov     rax, r14          // 返回 pthread_t 槽（调用方拿它去 join）
    jmp     .thread_create_done

.thread_create_fail_trampoline:
    mov     rdi, r14
    mov     esi, 8
    mov     edx, 8
    call    _zr_dealloc
.thread_create_fail:
    xor     eax, eax          // 返回 NULL（失败）

.thread_create_done:
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     rbp
    ret

#==============================================================================
# 线程 trampoline：把 zr 函数指针适配为 pthread_create 兼容的 C 入口
#------------------------------------------------------------------------------
# _zr_thread_trampoline(args)
#   args 指向 { func_ptr: *mut void, arg: *mut void }
#   pthread_create 要求 start_routine 是 C 约定的函数指针；zr 的函数指针
#   直接当 C 指针用会踩到 Mach-O 的符号存根问题，所以这里**间接调用**：
#   经由数据段里的函数指针跳转，链接器不需要为它生成存根。
#------------------------------------------------------------------------------
    .globl _zr_thread_trampoline
_zr_thread_trampoline:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    mov     rax, [rdi]        // func_ptr
    mov     rdi, [rdi + 8]    // arg
    call    rax               // 调用 zr 函数
    xor     rax, rax          // 返回 NULL
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_thread_join(thread, retval) -> i32
#   rdi = thread（zr_thread_create 返回的槽）, rsi = retval (*mut void)
# 等待线程结束，将返回值存入 *retval，返回 0 成功。
#------------------------------------------------------------------------------
    .globl _zr_thread_join
_zr_thread_join:
    push    rbp
    mov     rbp, rsp
    push    r12
    push    r13
    # 3 次 push 之后 rsp ≡ 0 (mod 16)（入口 8 − 3×8 = −16）；再减去 16 字节
    # 存放 pthread_join 的返回值槽，仍保持 16 字节对齐 —— System V AMD64 ABI
    # 要求"call 之前 rsp ≡ 0 (mod 16)"，否则 libc 里的 movaps 会段错误。
    sub     rsp, 16

    mov     r12, rdi          // thread ptr
    mov     r13, rsi          // retval ptr

    mov     rdi, [r12]        // pthread_t
    mov     rsi, rsp          // 临时存储 retval
    call    _pthread_join
    test    rax, rax
    jz      .join_success
    mov     rax, -1
    jmp     .join_done

.join_success:
    test    r13, r13
    jz      .join_no_retval
    mov     rax, [rsp]
    mov     [r13], rax
.join_no_retval:
    xor     eax, eax

.join_done:
    add     rsp, 16
    pop     r13
    pop     r12
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_thread_self() -> u64
# 返回 0
//------------------------------------------------------------------------------
    .globl _zr_thread_self
_zr_thread_self:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _pthread_self
    mov     rsp, rbp
    pop     rbp
    ret

#==============================================================================
# 进程支持
//------------------------------------------------------------------------------
# zr_process_fork() -> i64
# 返回：父进程返回子进程 PID，子进程返回 0，出错返回 -1
//------------------------------------------------------------------------------
    .globl _zr_process_fork
_zr_process_fork:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _fork
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_process_exec(path, argv, envp) -> !
#   rdi = path, rsi = argv, rdx = envp
# 不返回（成功时替换当前进程），出错返回 -1
//------------------------------------------------------------------------------
    .globl _zr_process_exec
_zr_process_exec:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _execve
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_process_wait(pid, status_ptr, options) -> i64
#   rdi = pid, rsi = status_ptr, rdx = options
# 返回等待的子进程 PID，出错返回 -1
//------------------------------------------------------------------------------
    .globl _zr_process_wait
_zr_process_wait:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    // 走 libc 的 waitpid：macOS 10.12 起 `syscall(2)` 已被标记为不支持
    // （直接调系统调用号拿不到正确结果），waitpid 才是可用的接口。
    // waitpid(pid, status, options) 的参数顺序与 wait4 前三个一致。
    call    _waitpid
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_process_getpid() -> i32
//------------------------------------------------------------------------------
    .globl _zr_process_getpid
_zr_process_getpid:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    qword ptr [_getpid_ptr + rip]
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# 任务队列：单线程执行器。
#   zr_spawn(frame)      把 future 帧挂到队尾，节点 = {frame, next}。
#   zr_run_tasks()       轮询所有任务；Ready 后调用帧头 drop 并释放节点。
# 帧头布局由 async_lower 固定：state=0, result=8, poll=16, drop=24。
//------------------------------------------------------------------------------
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
#==============================================================================
# 文件系统 / 路径 / 时间原语
#
# 全部走 libSystem。约定（与 zr 侧 std/fs.zr、std/path.zr 一致）：
#   成功返回 >= 0，失败返回 -1。
# 每个封装都在 `call` 前把 rsp 对齐到 16（libc 里的 movaps 需要）。
#==============================================================================

#------------------------------------------------------------------------------
# zr_mkdir(path) -> i32
#   rdi = path，权限固定 0777（受 umask 影响）
#------------------------------------------------------------------------------
    .globl _zr_mkdir
_zr_mkdir:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    mov     esi, 511            // mode = 0777
    call    _mkdir
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_rmdir(path) -> i32
#------------------------------------------------------------------------------
    .globl _zr_rmdir
_zr_rmdir:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _rmdir
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_unlink(path) -> i32
#------------------------------------------------------------------------------
    .globl _zr_unlink
_zr_unlink:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _unlink
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_rename(old, new) -> i32
#------------------------------------------------------------------------------
    .globl _zr_rename
_zr_rename:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _rename
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_path_exists(path) -> i32
#   存在返回 1，不存在返回 0。
#------------------------------------------------------------------------------
    .globl _zr_path_exists
_zr_path_exists:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 144
    mov     rsi, rsp
    call    _stat$INODE64
    test    eax, eax
    jnz     .exists_no
    mov     eax, 1
    jmp     .exists_done
.exists_no:
    xor     eax, eax
.exists_done:
    add     rsp, 144
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_is_dir(path) -> i32
#   是目录返回 1，不是返回 0，路径不存在返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_is_dir
_zr_is_dir:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 144
    mov     rsi, rsp
    call    _stat$INODE64
    test    eax, eax
    jnz     .isdir_err
    movzx   eax, word ptr [rsp+4]     // st_mode
    and     eax, 0xF000               // S_IFMT
    cmp     eax, 0x4000               // S_IFDIR
    sete    al
    movzx   eax, al
    jmp     .isdir_done
.isdir_err:
    mov     eax, -1
.isdir_done:
    add     rsp, 144
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_file_size(path) -> i64
#   返回文件字节数；不存在或出错返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_file_size
_zr_file_size:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 144
    mov     rsi, rsp
    call    _stat$INODE64
    test    eax, eax
    jnz     .fsize_err
    mov     rax, qword ptr [rsp+96]    // st_size
    jmp     .fsize_done
.fsize_err:
    mov     rax, -1
.fsize_done:
    add     rsp, 144
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_readdir_count(path) -> i64
#   目录项个数；出错返回 -1。（包含 "." 与 ".."）
#------------------------------------------------------------------------------
    .globl _zr_readdir_count
_zr_readdir_count:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 16
    call    _opendir
    test    rax, rax
    jz      .rdc_fail
    mov     qword ptr [rsp], rax       // DIR*
    mov     qword ptr [rsp+8], 0       // count
.rdc_loop:
    mov     rdi, qword ptr [rsp]
    call    _readdir$INODE64
    test    rax, rax
    jz      .rdc_end
    movzx   ecx, word ptr [rax+18]     // d_namlen（INODE64 布局）
    lea     rdx, [rax+21]              // d_name
    cmp     ecx, 1
    je      .rdc_dot
    cmp     ecx, 2
    je      .rdc_dotdot
    jmp     .rdc_count
.rdc_dot:
    cmp     byte ptr [rdx], 46         // '.'
    je      .rdc_loop
    jmp     .rdc_count
.rdc_dotdot:
    cmp     word ptr [rdx], 0x2E2E     // ".."
    je      .rdc_loop
.rdc_count:
    inc     qword ptr [rsp+8]
    jmp     .rdc_loop
.rdc_end:
    mov     rdi, qword ptr [rsp]
    call    _closedir
    mov     rax, qword ptr [rsp+8]
    jmp     .rdc_done
.rdc_fail:
    mov     rax, -1
.rdc_done:
    add     rsp, 16
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_readdir_name(path, index, buf, cap) -> i64
#   rdi = path, rsi = index, rdx = buf, rcx = cap
#   把第 index 个条目（跳过 "." 与 ".."）的名字写进 buf（**不含结尾 NUL**），
#   返回名字长度。出错（越界 / 打不开 / 名字比 cap 长）返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_readdir_name
_zr_readdir_name:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 64
    mov     qword ptr [rsp+8], rdx     // buf
    mov     qword ptr [rsp+16], rcx    // cap
    mov     qword ptr [rsp+24], rsi    // 剩余 index
    test    rsi, rsi
    js      .rdn_fail
    call    _opendir
    test    rax, rax
    jz      .rdn_fail
    mov     qword ptr [rsp], rax       // DIR*
.rdn_next:
    mov     rdi, qword ptr [rsp]
    call    _readdir$INODE64
    test    rax, rax
    jz      .rdn_close_fail
    movzx   ecx, word ptr [rax+18]     // d_namlen
    lea     rdx, [rax+21]              // d_name
    cmp     ecx, 1
    je      .rdn_dot
    cmp     ecx, 2
    je      .rdn_dotdot
    jmp     .rdn_cand
.rdn_dot:
    cmp     byte ptr [rdx], 46
    je      .rdn_next
    jmp     .rdn_cand
.rdn_dotdot:
    cmp     word ptr [rdx], 0x2E2E
    je      .rdn_next
.rdn_cand:
    cmp     qword ptr [rsp+24], 0
    je      .rdn_found
    dec     qword ptr [rsp+24]
    jmp     .rdn_next
.rdn_found:
    movzx   ecx, word ptr [rax+18]     // 名字长度
    mov     rdx, qword ptr [rsp+16]    // cap
    cmp     rcx, rdx
    ja      .rdn_close_fail            // 放不下
    mov     qword ptr [rsp+32], rcx
    mov     qword ptr [rsp+40], rax
    mov     rdi, qword ptr [rsp+8]
    mov     rsi, qword ptr [rsp+40]
    add     rsi, 21
    cld
    rep movsb
    mov     rdi, qword ptr [rsp]
    call    _closedir
    mov     rax, qword ptr [rsp+32]
    jmp     .rdn_done
.rdn_close_fail:
    mov     rdi, qword ptr [rsp]
    call    _closedir
.rdn_fail:
    mov     rax, -1
.rdn_done:
    add     rsp, 64
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
    .globl _zr_getcwd
_zr_getcwd:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _getcwd
    test    rax, rax
    jz      .gcwd_fail
    mov     rdi, rax
    call    _strlen
    jmp     .gcwd_done
.gcwd_fail:
    mov     rax, -1
.gcwd_done:
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_kill(pid, sig) -> i32
#------------------------------------------------------------------------------
    .globl _zr_kill
_zr_kill:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _kill
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_execvp(file, argv) -> i32
#   成功不返回；失败返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_execvp
_zr_execvp:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    call    _execvp
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_sleep_ms(ms) -> i32
#------------------------------------------------------------------------------
    .globl _zr_sleep_ms
_zr_sleep_ms:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    imul    rdi, rdi, 1000             // ms -> us
    call    _usleep
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_time_ns() -> i64     单调时钟，纳秒
#------------------------------------------------------------------------------
    .globl _zr_time_ns
_zr_time_ns:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 16
    mov     edi, 6                     // CLOCK_MONOTONIC
    mov     rsi, rsp
    call    _clock_gettime
    mov     rax, qword ptr [rsp]       // tv_sec
    imul    rax, rax, 1000000000
    add     rax, qword ptr [rsp+8]     // tv_nsec
    add     rsp, 16
    mov     rsp, rbp
    pop     rbp
    ret

#------------------------------------------------------------------------------
# zr_time_unix_ns() -> i64    墙上时钟，纳秒
#------------------------------------------------------------------------------
    .globl _zr_time_unix_ns
_zr_time_unix_ns:
    push    rbp
    mov     rbp, rsp
    and     rsp, -16
    sub     rsp, 16
    xor     edi, edi                   // CLOCK_REALTIME
    mov     rsi, rsp
    call    _clock_gettime
    mov     rax, qword ptr [rsp]
    imul    rax, rax, 1000000000
    add     rax, qword ptr [rsp+8]
    add     rsp, 16
    mov     rsp, rbp
    pop     rbp
    ret
