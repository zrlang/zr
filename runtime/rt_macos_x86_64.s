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