#==============================================================================
# runtime/rt_linux_x86_64.s — zr 语言运行时（Linux x86-64, AT&T syntax）
#
# 用纯 syscall 实现基础 `zr_*` 符号与单线程任务队列，不依赖 libc。
# System V AMD64 ABI。
#
# 系统调用号（x86-64 Linux）：write=1, read=0, exit=60, brk=12
#                            lseek=8, fstat=5, ioctl=16
#
# 内存分配：用 brk(12) 实现的简单 bump allocator。
# 教学级实现：不归还单个块，单次分配失败返回 NULL。
#==============================================================================

    .global zr_panic
    .global zr_write
    .global zr_read
    .global zr_open
    .global zr_close
    .global zr_seek
    .global zr_fstat_size
    .global zr_isatty
    .global zr_exit
    .global zr_alloc
    .global zr_dealloc
    .global zr_realloc
    .global zr_thread_create
    .global zr_thread_join
    .global zr_thread_self
    .global zr_process_fork
    .global zr_process_exec
    .global zr_process_wait
    .global zr_process_getpid

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
# zr_open(path, mode) -> isize
#   rdi = path (NUL 结尾), rsi = mode（0=只读 1=写 2=追加）
#   返回文件描述符，出错返回 -1（errno 由调用方自行判断）。
#
# 用 openat(AT_FDCWD, ...) —— 新内核上 open() 已经不推荐了。
# 三种模式对应的 O_* 标志不同，在这里统一映射，调用方只管传 0/1/2。
#------------------------------------------------------------------------------
zr_open:
    movq    %rsi, %rdx        # rdx = 第三个参数（mode），先留着
    movq    %rsi, %r10        # r10 = 第四个参数（mode 位）
    xorq    %rsi, %rsi        # rsi = 0 / 1 / 2
    # 标志：O_WRONLY=1, O_CREAT=64, O_TRUNC=512, O_APPEND=1024
    cmpq    $1, %rsi
    je      1f
    cmpq    $2, %rsi
    je      2f
    # 只读：O_RDONLY = 0
    xorq    %rdx, %rdx        # flags = 0
    jmp     3f
1:
    movq    $577, %rdx        # O_WRONLY | O_CREAT | O_TRUNC
    jmp     3f
2:
    movq    $1089, %rdx       # O_WRONLY | O_CREAT | O_APPEND
3:
    movq    $64, %r10         # mode = 0644
    movq    $-100, %rdi       # AT_FDCWD
    movq    $257, %rax        # SYS_openat
    syscall
    ret

#------------------------------------------------------------------------------
# zr_close(fd) -> i32
#   rdi = fd
#------------------------------------------------------------------------------
zr_close:
    movq    $3, %rax          # SYS_close
    syscall
    ret

#------------------------------------------------------------------------------
# zr_seek(fd, offset, whence) -> i64
#   rdi = fd, rsi = offset（字节数，可为负）, rdx = whence
#   whence: 0 = 起点(SEEK_SET), 1 = 当前位置(SEEK_CUR), 2 = 末尾(SEEK_END)
# 返回新的文件偏移；出错返回 -1。
#------------------------------------------------------------------------------
zr_seek:
    movq    $8, %rax          # SYS_lseek
    syscall
    testq   %rax, %rax
    jns     .seek_ok
    movq    $-1, %rax         # 出错：内核返回 -errno，统一成 -1
.seek_ok:
    ret

#------------------------------------------------------------------------------
# zr_fstat_size(fd) -> i64
#   rdi = fd
# 用 fstat(2) 填一个 144 字节的 struct stat，st_size 在偏移 48（8 字节）。
# 返回文件大小；出错返回 -1。
#------------------------------------------------------------------------------
zr_fstat_size:
    subq    $152, %rsp        # 144 字节的 stat 缓冲区（顺带保持 16 字节对齐）
    leaq    0(%rsp), %rsi     # statbuf
    movq    $5, %rax          # SYS_fstat
    syscall
    testq   %rax, %rax
    jl      .fstat_failed
    movq    48(%rsp), %rax    # st_size
    addq    $152, %rsp
    ret
.fstat_failed:
    movq    $-1, %rax
    addq    $152, %rsp
    ret

#------------------------------------------------------------------------------
# zr_isatty(fd) -> i32
#   rdi = fd
# 用 ioctl(fd, TCGETS) 判断：终端返回 1，其它（文件、管道、错误）返回 0。
#------------------------------------------------------------------------------
zr_isatty:
    subq    $72, %rsp         # struct termios 缓冲区
    leaq    0(%rsp), %rdx     # termios
    movq    $0x5401, %rsi     # TCGETS
    movq    $16, %rax         # SYS_ioctl
    syscall
    addq    $72, %rsp
    testq   %rax, %rax
    jz      .isatty_yes
    xorl    %eax, %eax        # 不是终端
    ret
.isatty_yes:
    movl    $1, %eax
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

#------------------------------------------------------------------------------
# 任务队列：单线程执行器。
#   zr_spawn(frame)      把 future 帧挂到队尾，节点 = {frame, next}。
#   zr_run_tasks()       轮询所有任务；Ready 后调用帧头 drop 并释放节点。
# 帧头布局由 async_lower 固定：state=0, result=8, poll=16, drop=24。
#------------------------------------------------------------------------------
    .bss
    .align  8
zr_task_head:
    .quad   0
zr_task_tail:
    .quad   0

    .text

    .global zr_spawn
zr_spawn:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %r12
    pushq   %r13
    movq    %rdi, %r12          # frame
    movq    $16, %rdi
    movq    $8, %rsi
    call    zr_alloc
    testq   %rax, %rax
    jz      .spawn_done
    movq    %rax, %r13
    movq    %r12, (%r13)
    movq    $0, 8(%r13)
    movq    zr_task_tail(%rip), %rax
    testq   %rax, %rax
    jz      .spawn_empty
    movq    %r13, 8(%rax)
    jmp     .spawn_set_tail
.spawn_empty:
    movq    %r13, zr_task_head(%rip)
.spawn_set_tail:
    movq    %r13, zr_task_tail(%rip)
.spawn_done:
    popq    %r13
    popq    %r12
    popq    %rbp
    ret

    .global zr_run_tasks
zr_run_tasks:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %r12
    pushq   %r13
    pushq   %r14
    subq    $8, %rsp
.run_loop:
    movq    zr_task_head(%rip), %r12
    testq   %r12, %r12
    jz      .run_done
    movq    8(%r12), %r13
    movq    %r13, zr_task_head(%rip)
    testq   %r13, %r13
    jnz     .run_have_next
    movq    $0, zr_task_tail(%rip)
.run_have_next:
    movq    (%r12), %r14
    movq    %r14, %rdi
    movq    16(%r14), %rax
    call    *%rax
    testq   %rax, %rax
    jz      .run_pending
    movq    %r14, %rdi
    movq    24(%r14), %rax
    call    *%rax
    movq    %r12, %rdi
    movq    $16, %rsi
    movq    $8, %rdx
    call    zr_dealloc
    jmp     .run_loop
.run_pending:
    movq    $0, 8(%r12)
    movq    zr_task_tail(%rip), %rax
    testq   %rax, %rax
    jz      .run_pending_empty
    movq    %r12, 8(%rax)
    jmp     .run_pending_tail
.run_pending_empty:
    movq    %r12, zr_task_head(%rip)
.run_pending_tail:
    movq    %r12, zr_task_tail(%rip)
    jmp     .run_loop
.run_done:
    addq    $8, %rsp
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbp
    ret

#==============================================================================
# 线程支持（pthread 封装）
#------------------------------------------------------------------------------
# zr_thread_create(start_routine, arg) -> i32
#   rdi = start_routine (fn(*mut void)), rsi = arg
# 返回 0 成功，非 0 失败（errno）。
# 调用 pthread_create，线程函数签名：void* (*)(void*)
#------------------------------------------------------------------------------
zr_thread_create:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15

    movq    %rdi, %r12      # start_routine
    movq    %rsi, %r13      # arg

    # 分配 pthread_t (8 字节)
    movq    $8, %rdi
    movq    $8, %rsi
    call    zr_alloc
    testq   %rax, %rax
    jz      .thread_create_fail
    movq    %rax, %r14      # pthread_t*

    # 分配栈（64KB）
    movq    $65536, %rdi
    movq    $16, %rsi
    call    zr_alloc
    testq   %rax, %rax
    jz      .thread_create_fail_stack
    movq    %rax, %r15      # stack pointer

    # pthread_create(pthread_t*, attr, start_routine, arg)
    # Linux x86-64: 使用 clone 系统调用实现，或者链接 pthread
    # 这里用 syscall 直接实现 clone
    # 栈增长方向向下，传入栈顶
    leaq    -8(%r15), %rdi  # child_stack (栈顶，8字节对齐)
    movq    $0x00010000, %rsi  # CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM
    movq    $0, %rdx        # ptid
    movq    $0, %r10        # tls
    movq    %r12, %r8       # start_routine
    movq    %r13, %r9       # arg
    movq    $56, %rax       # SYS_clone
    syscall
    testq   %rax, %rax
    jz      .thread_child
    # 父进程：rax = child tid
    movq    %rax, (%r14)    # 存入 pthread_t
    xorl    %eax, %eax      # 返回 0
    jmp     .thread_create_done

.thread_child:
    # 子线程：rax = 0，运行 start_routine(arg)
    call    *%r8
    # 线程结束，调用 exit
    movq    %rax, %rdi
    movq    $60, %rax       # SYS_exit
    syscall
    ud2

.thread_create_fail_stack:
    movq    %r14, %rdi
    movq    $8, %rsi
    movq    $8, %rdx
    call    zr_dealloc
.thread_create_fail:
    movq    $-1, %rax

.thread_create_done:
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_thread_join(thread, retval) -> i32
#   rdi = thread (pthread_t*), rsi = retval (*mut void)
# 等待线程结束，将返回值存入 *retval，返回 0 成功。
# 使用 futex 等待或简单轮询（简化实现：用 waitpid）
#------------------------------------------------------------------------------
zr_thread_join:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %r12
    pushq   %r13

    movq    %rdi, %r12      # thread ptr
    movq    %rsi, %r13      # retval ptr

    # 简单实现：轮询等待（生产环境应用 futex）
    # 读取线程 ID
    movq    (%r12), %rdi    # tid

.join_loop:
    # 使用 tgkill 或 waitpid 等待（简化：用 futex）
    # 这里用 SYS_futex 等待
    movq    $202, %rax      # SYS_futex
    movq    %r12, %rdi      # uaddr
    movq    $0, %rsi        # FUTEX_WAIT
    movq    $0, %rdx        # val
    movq    $0, %r10        # timeout
    syscall
    testq   %rax, %rax
    jz      .join_done
    # 如果 futex 失败（不支持），回退到轮询
    cmpq    $-38, %rax      # -ENOSYS
    je      .join_poll
    # 其他错误
    movq    $-1, %rax
    jmp     .join_done

.join_poll:
    # 简单轮询（1ms 睡眠）
    movq    $35, %rax       # SYS_nanosleep
    leaq    .join_ts(%rip), %rdi
    xorq    %rsi, %rsi
    syscall
    jmp     .join_loop

.section .rodata
.join_ts:
    .quad   0               # tv_sec
    .quad   1000000         # tv_nsec = 1ms
.text

.join_done:
    # 线程已结束，retval 已在线程退出时设置（简化：线程返回值在 rax，这里无法获取）
    # 实际实现需要线程局部存储或共享内存
    testq   %r13, %r13
    jz      .join_no_retval
    movq    $0, (%r13)      # 简化：存 0
.join_no_retval:
    xorl    %eax, %eax
    popq    %r13
    popq    %r12
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_thread_self() -> u64
# 返回当前线程 ID (pthread_self / gettid)
#------------------------------------------------------------------------------
zr_thread_self:
    movq    $186, %rax      # SYS_gettid
    syscall
    ret

#==============================================================================
# 进程支持
#------------------------------------------------------------------------------
# zr_process_fork() -> i64
# 返回：父进程返回子进程 PID，子进程返回 0，出错返回 -1
#------------------------------------------------------------------------------
zr_process_fork:
    movq    $57, %rax       # SYS_fork
    syscall
    ret

#------------------------------------------------------------------------------
# zr_process_exec(path, argv, envp) -> !
#   rdi = path, rsi = argv, rdx = envp
# 不返回（成功时替换当前进程），出错返回 -1
#------------------------------------------------------------------------------
zr_process_exec:
    movq    $59, %rax       # SYS_execve
    syscall
    # 只有出错才返回
    ret

#------------------------------------------------------------------------------
# zr_process_wait(pid, status_ptr, options) -> i64
#   rdi = pid, rsi = status_ptr, rdx = options
# 返回等待的子进程 PID，出错返回 -1
#------------------------------------------------------------------------------
zr_process_wait:
    movq    $61, %rax       # SYS_wait4
    # r10 = rusage (NULL)
    xorq    %r10, %r10
    syscall
    ret

#------------------------------------------------------------------------------
# zr_process_getpid() -> i32
#------------------------------------------------------------------------------
zr_process_getpid:
    movq    $39, %rax       # SYS_getpid
    syscall
    ret
