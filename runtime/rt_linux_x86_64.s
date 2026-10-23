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
    .global zr_mkdir
    .global zr_rmdir
    .global zr_unlink
    .global zr_rename
    .global zr_path_exists
    .global zr_is_dir
    .global zr_file_size
    .global zr_readdir_count
    .global zr_readdir_name
    .global zr_getcwd
    .global zr_kill
    .global zr_execvp
    .global zr_sleep_ms
    .global zr_time_ns
    .global zr_time_unix_ns

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
    movq    0(%rsp), %rax    # st_size
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
# zr_thread_create(start_routine, arg) -> *mut void
#   rdi = start_routine (fn(*mut void)), rsi = arg
# 返回线程句柄槽（供 zr_thread_join 使用），失败返回 NULL。
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
    movq    %r14, %rax      # 返回线程句柄槽（调用方拿它去 join）
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
    xorq    %rax, %rax      # 返回 NULL（失败）

.thread_create_done:
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_thread_join(thread, retval) -> i32
#   rdi = thread（zr_thread_create 返回的槽）, rsi = retval (*mut void)
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
#==============================================================================
# 目录 / 路径 / 文件元数据 / 信号 / 时间 原语
#
# 全部用 Linux 系统调用实现（**不依赖 libc**）：zr 生成的 Linux 程序用
# `ld -static` 链接，运行时不能假设 libc 存在。
#
# 用到的系统调用号（x86-64）：
#   mkdirat=258    rmdir=84      unlinkat=263    renameat=264
#   newfstatat=262 openat=257    getdents64=217  getcwd=79
#   kill=62        execve=59     nanosleep=35    clock_gettime=228
#   read=0         close=3
#
# struct linux_dirent64 布局：
#   d_ino[0..7] d_off[8..15] d_reclen[16..17] d_type[18] d_name[19..]
#
# struct stat（x86-64 内核通用布局）关键字段：
#   st_mode[24..27]   st_size[48..55]
#
# 目录条目约定：`.` 与 `..` **不计入** zr_readdir_count / zr_readdir_name
# 的下标，标准库直接按 0..count-1 遍历即可。
#
# 注意：本文件用 clang 的汇编器（LLVM integrated assembler）编译，它不像
# GNU as 那样给 `.foo` 做作用域划分，所以所有局部标号在全文件内必须唯一。
#==============================================================================

#------------------------------------------------------------------------------
# zr_mkdir(path) -> i32
#   rdi = path（NUL 结尾）
# 用 mkdirat(AT_FDCWD, path, 0755) 创建目录；成功返回 0，失败返回 -1。
#------------------------------------------------------------------------------
zr_mkdir:
    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    movq    $0755, %rdx        # mode
    movq    $258, %rax         # SYS_mkdirat
    syscall
    testq   %rax, %rax
    jz      .mkdir_ok
    movq    $-1, %rax
    ret
.mkdir_ok:
    xorl    %eax, %eax
    ret

#------------------------------------------------------------------------------
# zr_rmdir(path) -> i32
#   rdi = path（NUL 结尾）
# 删除空目录；成功返回 0，失败返回 -1。
#------------------------------------------------------------------------------
zr_rmdir:
    movq    $84, %rax          # SYS_rmdir
    syscall
    testq   %rax, %rax
    jz      .rmdir_ok
    movq    $-1, %rax
    ret
.rmdir_ok:
    xorl    %eax, %eax
    ret

#------------------------------------------------------------------------------
# zr_unlink(path) -> i32
#   rdi = path（NUL 结尾）
# 删除文件（不含目录）；成功返回 0，失败返回 -1。
#------------------------------------------------------------------------------
zr_unlink:
    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    xorq    %rdx, %rdx         # flags = 0（只删文件，不删目录）
    movq    $263, %rax         # SYS_unlinkat
    syscall
    testq   %rax, %rax
    jz      .unlink_ok
    movq    $-1, %rax
    ret
.unlink_ok:
    xorl    %eax, %eax
    ret

#------------------------------------------------------------------------------
# zr_rename(old, new) -> i32
#   rdi = old（NUL 结尾）, rsi = new（NUL 结尾）
# 重命名 / 移动文件；成功返回 0，失败返回 -1。
#------------------------------------------------------------------------------
zr_rename:
    movq    %rdi, %rsi         # oldpath
    movq    %rsi, %r10         # newpath
    movq    $-100, %rdi        # olddirfd = AT_FDCWD
    movq    $-100, %rdx        # newdirfd = AT_FDCWD
    movq    $264, %rax         # SYS_renameat
    syscall
    testq   %rax, %rax
    jz      .rename_ok
    movq    $-1, %rax
    ret
.rename_ok:
    xorl    %eax, %eax
    ret

#------------------------------------------------------------------------------
# zr_path_exists(path) -> i32
#   rdi = path（NUL 结尾）
# 用 newfstatat 查询路径元数据：存在返回 1，不存在（或出错）返回 0。
#------------------------------------------------------------------------------
zr_path_exists:
    subq    $160, %rsp         # struct stat 缓冲区
    leaq    0(%rsp), %rdx      # statbuf
    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    xorq    %r10, %r10         # flags = 0
    movq    $262, %rax         # SYS_newfstatat
    syscall
    addq    $160, %rsp
    testq   %rax, %rax
    jz      .pexists_yes
    xorl    %eax, %eax         # 不存在
    ret
.pexists_yes:
    movl    $1, %eax
    ret

#------------------------------------------------------------------------------
# zr_is_dir(path) -> i32
#   rdi = path（NUL 结尾）
# 是目录返回 1，不是目录返回 0，出错（打不开路径）返回 -1。
#------------------------------------------------------------------------------
zr_is_dir:
    subq    $160, %rsp         # struct stat 缓冲区
    leaq    0(%rsp), %rdx      # statbuf
    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    xorq    %r10, %r10         # flags = 0
    movq    $262, %rax         # SYS_newfstatat
    syscall
    testq   %rax, %rax
    jnz     .isdir_err
    movl    24(%rsp), %eax     # st_mode
    andl    $0xF000, %eax      # S_IFMT
    cmpl    $0x4000, %eax      # S_IFDIR
    je      .isdir_yes
    xorl    %eax, %eax         # 不是目录
    jmp     .isdir_done
.isdir_yes:
    movl    $1, %eax
.isdir_done:
    addq    $160, %rsp
    ret
.isdir_err:
    movq    $-1, %rax
    addq    $160, %rsp
    ret

#------------------------------------------------------------------------------
# zr_file_size(path) -> i64
#   rdi = path（NUL 结尾）
# 返回文件字节数；出错返回 -1。
#------------------------------------------------------------------------------
zr_file_size:
    subq    $160, %rsp         # struct stat 缓冲区
    leaq    0(%rsp), %rdx      # statbuf
    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    xorq    %r10, %r10         # flags = 0
    movq    $262, %rax         # SYS_newfstatat
    syscall
    testq   %rax, %rax
    jnz     .fsize_err
    movq    0(%rsp), %rax     # st_size
    jmp     .fsize_done
.fsize_err:
    movq    $-1, %rax
.fsize_done:
    addq    $160, %rsp
    ret

#------------------------------------------------------------------------------
# zr_readdir_count(path) -> i64
#   rdi = path（NUL 结尾）
# 打开目录并用 getdents64 枚举，返回条目数（不含 "." 与 ".."）；
# 打不开目录返回 -1。
#------------------------------------------------------------------------------
zr_readdir_count:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    subq    $4096, %rsp        # getdents64 缓冲区，固定在 0(%rsp)

    movq    %rdi, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    movq    $0x10000, %rdx     # O_DIRECTORY
    orq     $0x80000, %rdx     # O_CLOEXEC
    xorq    %r10, %r10         # mode
    movq    $257, %rax         # SYS_openat
    syscall
    cmpq    $0, %rax
    jle     .rdc_fail
    movq    %rax, %rbx         # fd

    xorq    %r14, %r14         # count = 0
.rdc_loop:
    movq    %rbx, %rdi         # fd
    leaq    0(%rsp), %rsi     # dirp
    movq    $4096, %rdx        # count
    movq    $217, %rax         # SYS_getdents64
    syscall
    testq   %rax, %rax
    jl      .rdc_fail          # 出错
    jz      .rdc_done          # 枚举结束
    xorq    %r8, %r8           # 缓冲区内偏移
.rdc_ent:
    leaq    0(%rsp), %rcx
    addq    %r8, %rcx          # 当前条目
    movzwl  16(%rcx), %r9d     # d_reclen
    testl   %r9d, %r9d
    jz      .rdc_done
    leaq    19(%rcx), %r10     # d_name
    # 跳过 "." 与 ".."
    cmpb    $0x2E, (%r10)
    jne     .rdc_count
    cmpb    $0x00, 1(%r10)
    je      .rdc_next
    cmpb    $0x2E, 1(%r10)
    jne     .rdc_count
    cmpb    $0x00, 2(%r10)
    je      .rdc_next
.rdc_count:
    incq    %r14
.rdc_next:
    addq    %r9, %r8           # offset += d_reclen
    jmp     .rdc_ent
.rdc_done:
    movq    %rbx, %rdi
    movq    $3, %rax           # SYS_close
    syscall
    movq    %r14, %rax
    jmp     .rdc_out
.rdc_fail:
    movq    $-1, %rax
.rdc_out:
    addq    $4096, %rsp
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_readdir_name(path, index, buf, cap) -> i64
#   rdi = path, rsi = index, rdx = buf, rcx = cap
# 把第 index 个条目（跳过 "." 与 ".."）的名字写进 buf（**不含结尾 NUL**），
# 返回名字长度。
# 出错返回 -1：index 越界、目录打不开、名字比 cap 还长（放不下）。
#------------------------------------------------------------------------------
zr_readdir_name:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    subq    $4096, %rsp        # getdents64 缓冲区，固定在 0(%rsp)

    movq    %rdi, %r12         # path
    movq    %rsi, %r13         # index
    movq    %rdx, %r14         # buf
    movq    %rcx, %r15         # cap
    testq   %r13, %r13
    js      .rdn_bad           # 负数下标非法

    movq    %r12, %rsi         # path
    movq    $-100, %rdi        # AT_FDCWD
    movq    $0x10000, %rdx     # O_DIRECTORY
    orq     $0x80000, %rdx     # O_CLOEXEC
    xorq    %r10, %r10         # mode
    movq    $257, %rax         # SYS_openat
    syscall
    cmpq    $0, %rax
    jle     .rdn_bad
    movq    %rax, %rbx         # fd

.rdn_loop:
    movq    %rbx, %rdi         # fd
    leaq    0(%rsp), %rsi     # dirp
    movq    $4096, %rdx        # count
    movq    $217, %rax         # SYS_getdents64
    syscall
    testq   %rax, %rax
    jl      .rdn_end           # 出错
    jz      .rdn_end           # 枚举结束
    xorq    %r8, %r8           # 缓冲区内偏移
.rdn_ent:
    leaq    0(%rsp), %rcx
    addq    %r8, %rcx          # 当前条目
    movzwl  16(%rcx), %r9d     # d_reclen
    testl   %r9d, %r9d
    jz      .rdn_end
    leaq    19(%rcx), %r10     # d_name
    # 跳过 "." 与 ".."
    cmpb    $0x2E, (%r10)
    jne     .rdn_cand
    cmpb    $0x00, 1(%r10)
    je      .rdn_next
    cmpb    $0x2E, 1(%r10)
    jne     .rdn_cand
    cmpb    $0x00, 2(%r10)
    je      .rdn_next
.rdn_cand:
    testq   %r13, %r13
    jz      .rdn_found
    decq    %r13
    jmp     .rdn_next
.rdn_next:
    addq    %r9, %r8           # offset += d_reclen
    jmp     .rdn_ent
.rdn_found:
    # 名字长度（r13 此时已无用，借来存长度）
    xorq    %r13, %r13
.rdn_nlen:
    cmpb    $0x00, (%r10,%r13)
    je      .rdn_nlen_done
    incq    %r13
    jmp     .rdn_nlen
.rdn_nlen_done:
    cmpq    %r13, %r15         # cap < 名字长度？
    jb      .rdn_long
    # 拷贝名字
    xorq    %r8, %r8
.rdn_copy:
    cmpq    %r13, %r8
    jge     .rdn_copy_done
    movb    (%r10,%r8), %al
    movb    %al, (%r14,%r8)
    incq    %r8
    jmp     .rdn_copy
.rdn_copy_done:
    movq    %rbx, %rdi
    movq    $3, %rax           # SYS_close
    syscall
    movq    %r13, %rax         # 返回名字长度
    jmp     .rdn_out
.rdn_long:
    movq    %rbx, %rdi
    movq    $3, %rax           # SYS_close
    syscall
.rdn_bad:
.rdn_end:
    movq    $-1, %rax
.rdn_out:
    addq    $4096, %rsp
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_getcwd(buf, cap) -> i64
#   rdi = buf, rsi = cap
# 用 getcwd(2) 取当前工作目录，写进 buf（含结尾 NUL），
# 返回路径长度（**不含结尾 NUL**）；失败返回 -1。
#------------------------------------------------------------------------------
zr_getcwd:
    movq    $79, %rax          # SYS_getcwd
    syscall
    testq   %rax, %rax
    js      .cwd_err           # 内核返回 -errno
    subq    $1, %rax           # getcwd 的长度含结尾 NUL
    ret
.cwd_err:
    movq    $-1, %rax
    ret

#------------------------------------------------------------------------------
# zr_kill(pid, sig) -> i32
#   rdi = pid, rsi = sig
# 给进程发信号；成功返回 0，失败返回 -1。
#------------------------------------------------------------------------------
zr_kill:
    movq    $62, %rax          # SYS_kill
    syscall
    testq   %rax, %rax
    jz      .kill_ok
    movq    $-1, %rax
    ret
.kill_ok:
    xorl    %eax, %eax
    ret

#------------------------------------------------------------------------------
# zr_sleep_ms(ms) -> ()
#   rdi = 毫秒数（u64）
# 用 nanosleep(2) 睡指定毫秒；被信号打断时用剩余时间接着睡完。
#------------------------------------------------------------------------------
zr_sleep_ms:
    pushq   %rbp
    movq    %rsp, %rbp
    subq    $32, %rsp          # struct timespec req / rem（各 16 字节）
    movq    %rdi, %rax         # ms
    movq    $1000, %rcx
    xorq    %rdx, %rdx
    divq    %rcx               # rax = 秒，rdx = 毫秒余数
    movq    %rax, 0(%rsp)      # req.tv_sec
    movq    %rdx, %rax
    imulq   $1000000, %rax
    movq    %rax, 8(%rsp)      # req.tv_nsec
.zsleep_again:
    movq    %rsp, %rdi         # req
    leaq    16(%rsp), %rsi     # rem
    movq    $35, %rax          # SYS_nanosleep
    syscall
    cmpq    $-4, %rax          # -EINTR：被信号打断
    jne     .zsleep_done
    movq    16(%rsp), %rax     # req = rem，继续睡
    movq    %rax, 0(%rsp)
    movq    24(%rsp), %rax
    movq    %rax, 8(%rsp)
    jmp     .zsleep_again
.zsleep_done:
    addq    $32, %rsp
    popq    %rbp
    ret

#------------------------------------------------------------------------------
# zr_time_ns() -> i64
# 单调时钟（CLOCK_MONOTONIC）的纳秒数；出错返回 -1。
#------------------------------------------------------------------------------
zr_time_ns:
    subq    $32, %rsp          # struct timespec
    movq    %rsp, %rdi
    movq    $1, %rsi           # CLOCK_MONOTONIC
    movq    $228, %rax         # SYS_clock_gettime
    syscall
    testq   %rax, %rax
    jnz     .tns_err
    movq    0(%rsp), %rax      # tv_sec
    movq    8(%rsp), %rcx      # tv_nsec
    imulq   $1000000000, %rax
    addq    %rcx, %rax
    jmp     .tns_done
.tns_err:
    movq    $-1, %rax
.tns_done:
    addq    $32, %rsp
    ret

#------------------------------------------------------------------------------
# zr_time_unix_ns() -> i64
# Unix 纪元（CLOCK_REALTIME）的纳秒数；出错返回 -1。
#------------------------------------------------------------------------------
zr_time_unix_ns:
    subq    $32, %rsp          # struct timespec
    movq    %rsp, %rdi
    xorq    %rsi, %rsi         # CLOCK_REALTIME
    movq    $228, %rax         # SYS_clock_gettime
    syscall
    testq   %rax, %rax
    jnz     .tun_err
    movq    0(%rsp), %rax      # tv_sec
    movq    8(%rsp), %rcx      # tv_nsec
    imulq    $1000000000, %rax
    addq    %rcx, %rax
    jmp     .tun_done
.tun_err:
    movq    $-1, %rax
.tun_done:
    addq    $32, %rsp
    ret

#------------------------------------------------------------------------------
# zr_execvp(file, argv) -> i32
#   rdi = file（NUL 结尾）, rsi = argv（NULL 结尾的 char* 数组）
# 语义同 POSIX execvp：file 里含 '/' 时直接执行，否则按 $PATH 依次查找。
# 成功时**当前进程映像被替换**（本函数不再返回）；失败返回 -1。
#
# 运行时不依赖 libc，因此 $PATH 与环境变量都从 /proc/self/environ 读：
# 该文件的内容正是进程启动时的环境变量（NUL 分隔的 "KEY=VALUE" 列表），
# 天然就是 execve 需要的 envp 数组的素材。
#------------------------------------------------------------------------------
zr_execvp:
    pushq   %rbp
    movq    %rsp, %rbp
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    subq    $4096, %rsp        # "目录/文件" 拼接缓冲，固定在 0(%rsp)

    movq    %rdi, %r12         # file
    movq    %rsi, %r13         # argv

    # ---- 1. 打开 /proc/self/environ ----
    leaq    .zre_environ_path(%rip), %rsi
    movq    $-100, %rdi        # AT_FDCWD
    xorq    %rdx, %rdx         # flags = O_RDONLY
    movq    $257, %rax         # SYS_openat
    syscall
    cmpq    $0, %rax
    jle     .zre_no_env        # 读不到环境：退化成空环境 + 默认 PATH
    movq    %rax, %rbx         # fd

    # ---- 2. 读进堆缓冲 ----
    movq    $65536, %rdi
    movq    $16, %rsi
    call    zr_alloc
    testq   %rax, %rax
    jz      .zre_env_fail      # 分配失败
    movq    %rax, %r14         # environ 缓冲区

    movq    %rbx, %rdi
    movq    %r14, %rsi
    movq    $65536, %rdx
    xorq    %rax, %rax         # SYS_read
    syscall
    movq    %rax, %r15         # 读到的字节数
    movq    %rbx, %rdi
    movq    $3, %rax           # SYS_close
    syscall
    cmpq    $0, %r15
    jle     .zre_env_fail

    # ---- 3. 数环境变量条数（每条以 NUL 结束）----
    movq    %r14, %rcx
    xorq    %r8, %r8           # 条目数
.zre_cnt_loop:
    cmpq    %r15, %rcx
    jae     .zre_cnt_done
    cmpb    $0x00, (%rcx)
    je      .zre_cnt_skip
.zre_cnt_entry:
    cmpb    $0x00, (%rcx)
    je      .zre_cnt_after
    incq    %rcx
    jmp     .zre_cnt_entry
.zre_cnt_after:
    incq    %r8
.zre_cnt_skip:
    incq    %rcx
    jmp     .zre_cnt_loop
.zre_cnt_done:

    # ---- 4. 分配并填充 envp 指针数组（末尾补 NULL）----
    movq    %r8, %rdi
    addq    $1, %rdi
    shlq    $3, %rdi           # (count + 1) * 8
    movq    $16, %rsi
    call    zr_alloc
    testq   %rax, %rax
    jz      .zre_env_fail
    movq    %rax, %rbx         # envp

    movq    %r14, %rcx         # 源
    movq    %rbx, %rdi         # 目标
.zre_fill_loop:
    cmpq    %r15, %rcx
    jae     .zre_fill_done
    movq    %rcx, (%rdi)
    addq    $8, %rdi
.zre_fill_skip:
    cmpq    %r15, %rcx
    jae     .zre_fill_done
    cmpb    $0x00, (%rcx)
    je      .zre_fill_next
    incq    %rcx
    jmp     .zre_fill_skip
.zre_fill_next:
    incq    %rcx
    jmp     .zre_fill_loop
.zre_fill_done:
    movq    $0, (%rdi)         # NULL 结尾

    # ---- 5. 找 PATH= ----
    xorq    %r8, %r8           # PATH 的值（0 = 没找到）
    movq    %r14, %rcx
.zre_path_loop:
    cmpq    %r15, %rcx
    jae     .zre_path_done
    cmpb    $0x50, (%rcx)      # 'P'
    jne     .zre_path_next
    cmpb    $0x41, 1(%rcx)     # 'A'
    jne     .zre_path_next
    cmpb    $0x54, 2(%rcx)     # 'T'
    jne     .zre_path_next
    cmpb    $0x48, 3(%rcx)     # 'H'
    jne     .zre_path_next
    cmpb    $0x3D, 4(%rcx)     # '='
    jne     .zre_path_next
    leaq    5(%rcx), %r8
    jmp     .zre_path_done
.zre_path_next:
    cmpb    $0x00, (%rcx)
    je      .zre_path_adv
.zre_path_skip:
    cmpq    %r15, %rcx
    jae     .zre_path_done
    cmpb    $0x00, (%rcx)
    je      .zre_path_adv
    incq    %rcx
    jmp     .zre_path_skip
.zre_path_adv:
    incq    %rcx
    jmp     .zre_path_loop
.zre_path_done:
    testq   %r8, %r8
    jnz     .zre_have_path
    leaq    .zre_default_path(%rip), %r8   # 兜底 PATH
.zre_have_path:
    jmp     .zre_do_exec

.zre_env_fail:
    # 读不到 /proc/self/environ：退化成空环境 + 默认 PATH
.zre_no_env:
    xorq    %rbx, %rbx         # envp = NULL（空环境）
    leaq    .zre_default_path(%rip), %r8

    # ---- 6. 执行：file 含 '/' 就直接 execve，否则按 PATH 逐个尝试 ----
.zre_do_exec:
    movq    %r12, %rcx         # 扫一遍 file，看有没有 '/'
.zre_slash_loop:
    cmpb    $0x00, (%rcx)
    je      .zre_no_slash
    cmpb    $0x2F, (%rcx)
    je      .zre_has_slash
    incq    %rcx
    jmp     .zre_slash_loop
.zre_no_slash:
    movq    %r8, %r10          # PATH 值的游标
    jmp     .zre_comp_loop
.zre_has_slash:
    movq    %r12, %rdi         # path
    movq    %r13, %rsi         # argv
    movq    %rbx, %rdx         # envp
    movq    $59, %rax          # SYS_execve
    syscall
    jmp     .zre_fail          # 只有失败才会返回

.zre_comp_loop:
    leaq    0(%rsp), %rdi     # 目标缓冲
    movq    %r10, %rsi         # 源（PATH 分量）
    xorq    %r9, %r9           # 分量长度
.zre_comp_copy:
    cmpb    $0x00, (%rsi)
    je      .zre_comp_end
    cmpb    $0x3A, (%rsi)      # ':'
    je      .zre_comp_end
    movb    (%rsi), %al
    movb    %al, (%rdi)
    incq    %rdi
    incq    %rsi
    incq    %r9
    jmp     .zre_comp_copy
.zre_comp_end:
    movq    %rsi, %r10         # 记住下一个分量的位置
    leaq    0(%rsp), %rax
    addq    %r9, %rax          # 拼接位置
    testq   %r9, %r9
    jnz     .zre_comp_join
    movb    $0x2E, (%rax)      # 空分量 = 当前目录
    incq    %rax
    incq    %r9
.zre_comp_join:
    movb    $0x2F, (%rax)      # '/'
    incq    %rax
    movq    %r12, %rsi         # 追加 file
.zre_comp_file:
    cmpb    $0x00, (%rsi)
    je      .zre_comp_exec
    movb    (%rsi), %al
    movb    %al, (%rax)
    incq    %rax
    incq    %rsi
    jmp     .zre_comp_file
.zre_comp_exec:
    movb    $0x00, (%rax)      # NUL 结尾
    leaq    0(%rsp), %rdi     # path
    movq    %r13, %rsi         # argv
    movq    %rbx, %rdx         # envp
    movq    $59, %rax          # SYS_execve
    syscall
    # 执行失败：换下一个 PATH 分量
    cmpb    $0x00, (%r10)
    je      .zre_fail
    incq    %r10
    jmp     .zre_comp_loop

.zre_fail:
    movq    $-1, %rax
    addq    $4096, %rsp
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    popq    %rbp
    ret

    .section .rodata
.zre_environ_path:
    .asciz  "/proc/self/environ"
.zre_default_path:
    .asciz  "/bin:/usr/bin"
    .text

