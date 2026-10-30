#==============================================================================
# runtime/rt_macos_arm64.s — zr 语言运行时（macOS aarch64 / arm64, AT&T syntax）
#
# 与 x86-64 版一一对应：libc 薄封装。arm64 macOS 上 C 符号同样带一个
# 前导下划线（`write` → `_write`）。
#
# AAPCS64（Darwin）：参数 x0..x7，返回值 x0；
# 调用前无需 shadow space，但栈必须 16 字节对齐（bl 自身会保存 x30）。
#==============================================================================

    .section __TEXT,__text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   x0 = msg, x1 = len
# 写 stderr(fd=2)，退出码 99。
#------------------------------------------------------------------------------
    .globl _zr_panic
_zr_panic:
    stp     x29, x30, [sp, #-32]!
    mov     x29, sp
    str     x1, [sp, #16]       // 保护 len（write 可能改写 caller-save 寄存器）
    mov     x2, x1              // count = len
    mov     x1, x0              // buf = msg
    mov     x0, #2              // fd = stderr
    bl      _write
    mov     x0, #99             // panic exit code
    bl      _exit
    ldp     x29, x30, [sp], #32
    ret

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#   x0=fd, x1=buf, x2=count
#------------------------------------------------------------------------------
    .globl _zr_write
_zr_write:
    b       _write

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#------------------------------------------------------------------------------
    .globl _zr_read
_zr_read:
    b       _read

#------------------------------------------------------------------------------
# zr_open(path, mode) -> isize
#   x0 = path (NUL 结尾), x1 = mode（0=只读 1=写 2=追加）
#
# macOS 的 O_* 标志与 Linux 不同（O_CREAT = 0x200），这里统一成调用方的
# 0/1/2 语义：只读、写（截断）、追加。
#------------------------------------------------------------------------------
    .globl _zr_open
_zr_open:
    stp     x29, x30, [sp, #-16]!
    mov     x29, sp
    mov     x8, x1             // x8 = mode
    cmp     x8, 0
    b.eq    2f                 // 只读
    cmp     x8, 1
    b.eq    1f                 // 写（截断）
    // 追加：O_WRONLY | O_CREAT | O_APPEND = 0x1 | 0x200 | 0x8
    mov     x1, 0x209
    b       3f
1:
    // 写（截断）：O_WRONLY | O_CREAT | O_TRUNC = 0x1 | 0x200 | 0x400
    mov     x1, 0x601
    b       3f
2:
    // 只读：O_RDONLY
    mov     x1, 0
3:
    mov     x2, 0644           // mode
    bl      _open
    ldp     x29, x30, [sp], #16
    ret

#------------------------------------------------------------------------------
# zr_close(fd) -> i32
#   x0 = fd
#------------------------------------------------------------------------------
    .globl _zr_close
_zr_close:
    b       _close

#------------------------------------------------------------------------------
# zr_seek(fd, offset, whence) -> i64
#   x0=fd, x1=offset（字节数，可为负）, x2=whence
#   whence: 0 = 起点(SEEK_SET), 1 = 当前位置(SEEK_CUR), 2 = 末尾(SEEK_END)
# lseek 的参数顺序与 C 一致，SEEK_SET/CUR/END 也正好是 0/1/2；
# 出错时 libc 返回 -1，正是我们对外的约定。
#------------------------------------------------------------------------------
    .globl _zr_seek
_zr_seek:
    b       _lseek

#------------------------------------------------------------------------------
# zr_fstat_size(fd) -> i64
#   x0=fd
# macOS 的 struct stat 是 144 字节，st_size 在偏移 96（8 字节）。
# arm64 上只有 64 位 inode 一套 ABI，libc 的符号就是 `fstat`。
# 返回文件大小；出错返回 -1。
#------------------------------------------------------------------------------
    .globl _zr_fstat_size
_zr_fstat_size:
    stp     x29, x30, [sp, #-32]!
    mov     x29, sp
    sub     sp, sp, #144       // 144 字节的 stat 缓冲区（sp 保持 16 字节对齐）
    mov     x1, sp             // statbuf
    bl      _fstat
    cmp     x0, #0
    b.ne    .fstat_failed      // fstat 成功返回 0
    ldr     x0, [sp, #96]      // st_size
    b       .fstat_done
.fstat_failed:
    mov     x0, #-1
.fstat_done:
    add     sp, sp, #144
    ldp     x29, x30, [sp], #32
    ret

#------------------------------------------------------------------------------
# zr_isatty(fd) -> i32
#   x0=fd
# libc 的 isatty：终端返回非 0，其它返回 0；这里统一成 1 / 0。
#------------------------------------------------------------------------------
    .globl _zr_isatty
_zr_isatty:
    stp     x29, x30, [sp, #-16]!
    mov     x29, sp
    bl      _isatty
    cmp     x0, #0
    cset    w0, ne              // 终端 -> 1，其它 -> 0
    ldp     x29, x30, [sp], #16
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#------------------------------------------------------------------------------
    .globl _zr_exit
_zr_exit:
    b       _exit

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#   x0 = size, x1 = align（malloc 只用 size）
#------------------------------------------------------------------------------
    .globl _zr_alloc
_zr_alloc:
    b       _malloc

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
#   x0 = ptr
#------------------------------------------------------------------------------
    .globl _zr_dealloc
_zr_dealloc:
    b       _free

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   x0 = ptr, x1 = old, x2 = new, x3 = align
#------------------------------------------------------------------------------
    .globl _zr_realloc
_zr_realloc:
    mov     x1, x2              // realloc(ptr, new_size)
    b       _realloc
#==============================================================================
# 文件系统 / 路径 / 时间原语：本平台暂未实现，统一返回 -1。
# 符号必须存在，否则 `use fs` 的程序链接失败。
#==============================================================================
    .text

    .globl _zr_mkdir
_zr_mkdir:
    mov     x0, #-1
    ret

    .globl _zr_rmdir
_zr_rmdir:
    mov     x0, #-1
    ret

    .globl _zr_unlink
_zr_unlink:
    mov     x0, #-1
    ret

    .globl _zr_rename
_zr_rename:
    mov     x0, #-1
    ret

    .globl _zr_path_exists
_zr_path_exists:
    mov     x0, #-1
    ret

    .globl _zr_is_dir
_zr_is_dir:
    mov     x0, #-1
    ret

    .globl _zr_file_size
_zr_file_size:
    mov     x0, #-1
    ret

    #------------------------------------------------------------------------------
# zr_file_mtime(path) -> i64
#   该平台未实现，返回 -1（与 zr_file_size 的空实现一致）。
#------------------------------------------------------------------------------
    .globl _zr_file_mtime
_zr_file_mtime:
    mov     x0, #-1
    ret

#------------------------------------------------------------------------------
# zr_file_mode(path) -> i64
#   该平台未实现，返回 -1（与 zr_file_size 的空实现一致）。
#------------------------------------------------------------------------------
    .globl _zr_file_mode
_zr_file_mode:
    mov     x0, #-1
    ret
.globl _zr_readdir_count
_zr_readdir_count:
    mov     x0, #-1
    ret

    .globl _zr_readdir_name
_zr_readdir_name:
    mov     x0, #-1
    ret

    .globl _zr_getcwd
_zr_getcwd:
    mov     x0, #-1
    ret

    .globl _zr_kill
_zr_kill:
    mov     x0, #-1
    ret

    .globl _zr_execvp
_zr_execvp:
    mov     x0, #-1
    ret

    .globl _zr_sleep_ms
_zr_sleep_ms:
    mov     x0, #-1
    ret

    .globl _zr_time_ns
_zr_time_ns:
    mov     x0, #-1
    ret

    .globl _zr_time_unix_ns
_zr_time_unix_ns:
    mov     x0, #-1
    ret

#==============================================================================
# BSD 套接字封装（net.zr 的 extern "C" 对接 libSystem，arm64 版）
#==============================================================================
# 这些是薄封装：参数已经在 AAPCS64 的 x0..x7 里，直接尾调用 libc 函数。
# 尾调用用 `b`（不修改 x30，被调函数返回时直接回到本函数调用者）。
# macOS 的 BSD 套接字函数都在 libSystem 里，符号带前导下划线。

# zr_socket(domain, type, protocol) -> fd
    .globl _zr_socket
_zr_socket:
    b       _socket

# zr_bind(fd, addr, addrlen) -> i32
    .globl _zr_bind
_zr_bind:
    b       _bind

# zr_connect(fd, addr, addrlen) -> i32
    .globl _zr_connect
_zr_connect:
    b       _connect

# zr_listen(fd, backlog) -> i32
    .globl _zr_listen
_zr_listen:
    b       _listen

# zr_accept(fd, addr, addrlen) -> fd
    .globl _zr_accept
_zr_accept:
    b       _accept

# zr_recv(fd, buf, len, flags) -> isize
    .globl _zr_recv
_zr_recv:
    b       _recv

# zr_send(fd, buf, len, flags) -> isize
    .globl _zr_send
_zr_send:
    b       _send

# zr_setsockopt(fd, level, optname, optval, optlen) -> i32
    .globl _zr_setsockopt
_zr_setsockopt:
    b       _setsockopt

# zr_getsockopt(fd, level, optname, optval, optlen) -> i32
    .globl _zr_getsockopt
_zr_getsockopt:
    b       _getsockopt

# zr_getaddrinfo(node, service, hints, res) -> i32
    .globl _zr_getaddrinfo
_zr_getaddrinfo:
    b       _getaddrinfo

# zr_freeaddrinfo(res)
    .globl _zr_freeaddrinfo
_zr_freeaddrinfo:
    b       _freeaddrinfo

# zr_inet_ntop(af, src, dst, size) -> *const u8
    .globl _zr_inet_ntop
_zr_inet_ntop:
    b       _inet_ntop

# zr_inet_pton(af, src, dst) -> i32
    .globl _zr_inet_pton
_zr_inet_pton:
    b       _inet_pton

# zr_htons(x) -> u16
    .globl _zr_htons
_zr_htons:
    b       _htons

# zr_ntohs(x) -> u16
    .globl _zr_ntohs
_zr_ntohs:
    b       _ntohs

# zr_htonl(x) -> u32
    .globl _zr_htonl
_zr_htonl:
    b       _htonl

# zr_ntohl(x) -> u32
    .globl _zr_ntohl
_zr_ntohl:
    b       _ntohl

# zr_set_nonblocking(fd, nonblocking) -> i32
#   x0=fd, x1=nonblocking（非 0 = 置 O_NONBLOCK）
# 两次 fcntl（F_GETFL=3 / F_SETFL=4），fd 与标志跨调用用 callee-saved x19/x20。
    .globl _zr_set_nonblocking
_zr_set_nonblocking:
    stp     x29, x30, [sp, #-32]!
    mov     x29, sp
    str     x19, [sp, #16]
    str     x20, [sp, #24]
    mov     x19, x0             // fd
    mov     x20, x1             // nonblocking
    mov     x0, x19
    mov     x1, #3              // F_GETFL
    bl      _fcntl
    mov     x2, x0              // 当前 flags
    cmp     x20, #0
    b.eq    1f
    orr     x2, x2, #4          // 置 O_NONBLOCK（macOS 上 O_NONBLOCK = 0x4）
    b       2f
1:
    bic     x2, x2, #4          // 清 O_NONBLOCK（~4，bic 按位清）
2:
    mov     x0, x19
    mov     x1, #4              // F_SETFL
    bl      _fcntl
    ldr     x19, [sp, #16]
    ldr     x20, [sp, #24]
    ldp     x29, x30, [sp], #32
    ret

# zr_errno() -> i32
#   macOS 的 errno 是线程局部存储，经 `__error()` 取地址；返回其值。
    .globl _zr_errno
_zr_errno:
    stp     x29, x30, [sp, #-16]!
    mov     x29, sp
    bl      ___error
    ldr     w0, [x0]
    ldp     x29, x30, [sp], #16
    ret
