#==============================================================================
# runtime/rt_linux_aarch64.s — zr 语言运行时（Linux aarch64, AT&T syntax）
#
# 用纯 syscall 实现 7 个 `zr_*` 符号，不依赖 libc。
# AArch64 过程调用标准（AAPCS64）：
#   参数 x0..x7，返回值 x0，callee-save x19..x28，x29=FP，x30=LR，x31=SP
#
# 系统调用号（aarch64 Linux）：
#   read=63, write=64, exit=93, brk=214
#   调用方式：系统调用号放 x8，参数 x0..x5，`svc #0`
#
# 内存分配：用 brk(214) 实现的简单 bump allocator。
# 教学级实现：不归还单个块，单次分配失败返回 NULL(0)。
#==============================================================================

    .global zr_panic
    .global zr_write
    .global zr_read
    .global zr_exit
    .global zr_alloc
    .global zr_dealloc
    .global zr_realloc

    .section .text

#------------------------------------------------------------------------------
# zr_panic(msg, len) -> !
#   x0 = msg, x1 = len
# 写 stderr(fd=2)，退出码 99。
#------------------------------------------------------------------------------
zr_panic:
    mov     x2, x1              // count = len
    mov     x1, x0              // buf = msg
    mov     x0, #2              // fd = stderr
    mov     x8, #64             // __NR_write
    svc     #0
    mov     x0, #99             // panic exit code
    mov     x8, #93             // __NR_exit
    svc     #0
    brk     #0

#------------------------------------------------------------------------------
# zr_write(fd, buf, count) -> isize
#   x0=fd, x1=buf, x2=count
#------------------------------------------------------------------------------
zr_write:
    mov     x8, #64             // __NR_write
    svc     #0
    ret

#------------------------------------------------------------------------------
# zr_read(fd, buf, count) -> isize
#------------------------------------------------------------------------------
zr_read:
    mov     x8, #63             // __NR_read
    svc     #0
    ret

#------------------------------------------------------------------------------
# zr_exit(code) -> !
#------------------------------------------------------------------------------
zr_exit:
    mov     x8, #93             // __NR_exit
    svc     #0
    brk     #0

#------------------------------------------------------------------------------
# zr_alloc(size, align) -> *mut void
#
# Bump allocator：
#   heap_cursor / heap_end 是 bss 中的两个 8 字节变量。
#   首次调用用 brk(0) 取当前 program break，对齐到 16 字节。
#   若当前段不足，brk(heap_end + max(size, 64K)) 扩展。
#   成功返回对齐后的地址；失败返回 NULL(0)。
#------------------------------------------------------------------------------
    .bss
    .align  3
heap_cursor:
    .quad   0
heap_end:
    .quad   0

    .text

zr_alloc:
    stp     x29, x30, [sp, #-32]!
    mov     x29, sp

    // x8 = align_up(size, 16)
    add     x8, x0, #15
    and     x8, x8, #-16

    // if heap_cursor == 0 -> init
    adrp    x2, heap_cursor
    add     x2, x2, :lo12:heap_cursor
    ldr     x3, [x2]
    cbnz    x3, .have_cursor

    // brk(0) -> x0 = current program break
    mov     x0, #0
    mov     x8, #214            // __NR_brk
    svc     #0
    add     x0, x0, #15
    and     x0, x0, #-16
    str     x0, [x2]            // heap_cursor = heap_end = break
    adrp    x2, heap_end
    add     x2, x2, :lo12:heap_end
    str     x0, [x2]

.have_cursor:
    // if cursor + need <= heap_end, 直接分配
    ldr     x3, [x2]            // cursor（x2 仍指向 heap_cursor）
    add     x9, x3, x8
    adrp    x2, heap_end
    add     x2, x2, :lo12:heap_end
    ldr     x10, [x2]
    cmp     x9, x10
    b.le    .fits

    // 需要扩展：new_end = align16(heap_end + max(need, 65536))
    ldr     x10, [x2]           // heap_end
    mov     x11, #65536
    cmp     x8, x11
    csel    x11, x8, x11, gt    // max(need, 64K)
    add     x10, x10, x11
    add     x10, x10, #15
    and     x10, x10, #-16
    mov     x0, x10
    mov     x8, #214            // __NR_brk
    svc     #0
    cmp     x0, x10
    b.lt    .alloc_fail
    str     x0, [x2]            // heap_end = new break

.fits:
    adrp    x2, heap_cursor
    add     x2, x2, :lo12:heap_cursor
    ldr     x0, [x2]            // 返回值 = cursor
    add     x3, x0, x8
    str     x3, [x2]            // cursor += size
    ldp     x29, x30, [sp], #32
    ret

.alloc_fail:
    mov     x0, #0
    ldp     x29, x30, [sp], #32
    ret

#------------------------------------------------------------------------------
# zr_dealloc(ptr, size, align)
# Bump allocator 不归还单块；此实现仅为接口完整性（无操作）。
#------------------------------------------------------------------------------
zr_dealloc:
    ret

#------------------------------------------------------------------------------
# zr_realloc(ptr, old_size, new_size, align) -> *mut void
#   x0=ptr, x1=old_size, x2=new_size, x3=align
# 若 new_size <= old_size：原地返回 ptr。
# 否则：分配新块、拷贝旧内容、返回新块（不释放旧块）。
#------------------------------------------------------------------------------
zr_realloc:
    cmp     x2, x1
    b.le    .realloc_inplace

    sub     sp, sp, #48
    stp     x29, x30, [sp, #16]
    str     x0, [sp]            // old ptr
    str     x1, [sp, #8]        // old size
    str     x2, [sp, #40]       // new size

    mov     x0, x2              // size = new_size
    mov     x1, x3              // align
    bl      zr_alloc
    mov     x19, x0             // new ptr

    // 拷贝 min(old_size, new_size) 字节
    ldr     x1, [sp, #8]
    ldr     x2, [sp, #40]
    cmp     x1, x2
    csel    x2, x1, x2, lt
    cbz     x2, .realloc_done
    ldr     x3, [sp]            // src
    mov     x4, x19             // dst

.realloc_copy:
    ldrb    w5, [x3], #1
    strb    w5, [x4], #1
    subs    x2, x2, #1
    b.ne    .realloc_copy

.realloc_done:
    mov     x0, x19
    ldr     x19, [sp, #32]
    ldp     x29, x30, [sp, #16]
    add     sp, sp, #48
    ret

.realloc_inplace:
    ret