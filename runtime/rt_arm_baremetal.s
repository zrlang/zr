#==============================================================================
# runtime/rt_arm_baremetal.s — zr 运行时（ARM 裸机，ARM semihosting）
#
# 裸机没有内核也没有 libc：输出走 **semihosting**（由仿真器 / 调试器
# 接管），内存来自一块静态堆。`zr_*` 符号与其它平台保持一致。
#
# AAPCS32：r0-r3 传参、r0 返回；r13=SP，r14=LR，r15=PC。
#
# semihosting 调用约定（ARM 32 位 A-profile）：
#     r0 = 操作号，r1-r3 = 参数
#     0x03 SYS_WRITEC  (r1 = 单个字符)
#     0x04 SYS_WRITE0   (r1 = 以 NUL 结尾的字符串)
#     0x18 SYS_EXIT     (r1 = 退出原因)
# 指令：bkpt 0xab。
#==============================================================================

    .syntax unified
    .text

    .global zr_panic
    .global zr_write
    .global zr_read
    .global zr_exit
    .global zr_alloc
    .global zr_dealloc
    .global zr_realloc

//------------------------------------------------------------------------------
// 静态堆：bump allocator 的唯一内存来源。
// 起始地址由链接脚本放置在 .data 之后；大小可通过 -DZR_HEAP_SIZE 覆盖。
//
// heap_cursor 必须在 .bss 里**真正占位**：zr_alloc 会读写它，
// 只声明成符号而不分配存储会让第一次分配从地址 0 开始。
//------------------------------------------------------------------------------
    .bss
    .align  4
heap_cursor:
    .word  0                         // 下一次分配的起点，首次由 zr_alloc 填入 heap_base
    .align  4
heap_base:
    .space 65536                     // ZR_HEAP_SIZE = 64 KiB
heap_end:
    .bss_size = heap_end - heap_base

    .text
    .align  2

//------------------------------------------------------------------------------
// zr_write(fd, buf, count) -> isize
//   r0 = fd, r1 = buf, r2 = count
// semihosting 没有"写缓冲区"操作，逐字节 SYS_WRITEC 输出；返回写入字节数。
//------------------------------------------------------------------------------
zr_write:
    push    {r4, r5, r6, lr}
    mov     r4, r1                  // buf
    mov     r5, r2                  // count
    mov     r6, #0                  // 已写入

1:  cmp     r5, #0
    beq     2f
    ldrb    r1, [r4], #1
    mov     r0, #3                  // SYS_WRITEC
    bkpt    #0xab
    add     r6, r6, #1
    subs    r5, r5, #1
    bne     1b

2:  mov     r0, r6                  // 返回写入字节数
    pop     {r4, r5, r6, lr}
    bx      lr

//------------------------------------------------------------------------------
// zr_read(fd, buf, count) -> isize
//   r0 = fd, r1 = buf, r2 = count
// 裸机没有输入来源：返回 0 表示"读到 EOF"。
//------------------------------------------------------------------------------
    .global zr_read
zr_read:
    mov     r0, #0
    bx      lr

//------------------------------------------------------------------------------
// zr_seek(fd, offset, whence) -> i64
//   r0 = fd, r1 = offset, r2 = whence
//   whence: 0 = 起点(SEEK_SET), 1 = 当前位置(SEEK_CUR), 2 = 末尾(SEEK_END)
// 裸机没有文件系统：返回 -1（不支持）。
//------------------------------------------------------------------------------
    .global zr_seek
zr_seek:
    mov     r0, #-1
    bx      lr

//------------------------------------------------------------------------------
// zr_fstat_size(fd) -> i64
//   r0 = fd
// 裸机没有文件系统：返回 -1（不支持）。
//------------------------------------------------------------------------------
    .global zr_fstat_size
zr_fstat_size:
    mov     r0, #-1
    bx      lr

//------------------------------------------------------------------------------
// zr_isatty(fd) -> i32
//   r0 = fd
// 裸机没有终端：返回 0。
//------------------------------------------------------------------------------
    .global zr_isatty
zr_isatty:
    mov     r0, #0
    bx      lr

//------------------------------------------------------------------------------
// zr_exit(code) -> !
//   r0 = code
//------------------------------------------------------------------------------
    .global zr_exit
zr_exit:
    mov     r1, r0                  // SYS_EXIT 的参数是 r1
    mov     r0, #0x18               // SYS_EXIT
    bkpt    #0xab
3:  b       3b                      // 调试器未接管时自旋

//------------------------------------------------------------------------------
// zr_panic(msg, len) -> !
//   r0 = msg, r1 = len
// 逐字节写 stderr（fd=2 由调用方语义决定，semihosting 只有一个输出通道）。
//------------------------------------------------------------------------------
    .global zr_panic
zr_panic:
    push    {r4, lr}
    mov     r4, r0                  // msg
    mov     r5, r1                  // len
1:  cmp     r5, #0
    beq     2f
    ldrb    r1, [r4], #1
    mov     r0, #3                  // SYS_WRITEC
    bkpt    #0xab
    subs    r5, r5, #1
    bne     1b
2:  mov     r0, #99
    b       zr_exit

//------------------------------------------------------------------------------
// zr_alloc(size, align) -> *mut void
//   r0 = size, r1 = align
// Bump allocator：对齐到 16 字节，耗尽返回 NULL(0)。
//------------------------------------------------------------------------------
    .global zr_alloc
zr_alloc:
    push    {r4, lr}
    ldr     r2, =heap_cursor
    ldr     r3, [r2]
    cmp     r3, #0
    bne     1f
    // 首次调用：用静态堆的起始地址初始化游标
    ldr     r3, =heap_base
    str     r3, [r2]

1:  add     r4, r3, r0              // cursor + size
    add     r4, r4, #15
    and     r4, r4, #-16           // 对齐到 16
    ldr     r12, =heap_end
    cmp     r4, r12
    bhi     2f                      // 堆耗尽

    str     r4, [r2]                // cursor = 返回地址
    mov     r0, r3
    pop     {r4, lr}
    bx      lr

2:  mov     r0, #0                  // 失败
    pop     {r4, lr}
    bx      lr

//------------------------------------------------------------------------------
// zr_dealloc(ptr, size, align)
// Bump allocator 不归还单块；保留接口完整性。
//------------------------------------------------------------------------------
    .global zr_dealloc
zr_dealloc:
    bx      lr

//------------------------------------------------------------------------------
// zr_realloc(ptr, old_size, new_size, align) -> *mut void
//   r0 = ptr, r1 = old_size, r2 = new_size, r3 = align
// new_size <= old_size 时原地返回；否则申请新块并拷贝旧内容。
//------------------------------------------------------------------------------
    .global zr_realloc
zr_realloc:
    push    {r4, r5, r6, r7, lr}
    mov     r4, r0                  // old ptr
    mov     r5, r1                  // old size
    mov     r6, r2                  // new size
    mov     r7, r3                  // align

    cmp     r6, r5
    bhi     1f                      // 需要搬迁
    mov     r0, r4                  // 原地返回
    pop     {r4, r5, r6, r7, lr}
    bx      lr

1:  mov     r0, r6                  // size = new_size
    mov     r1, r7                  // align
    bl      zr_alloc
    cmp     r0, #0
    beq     2f                      // 分配失败

    mov     r1, r0                  // new ptr
    // 拷贝 min(old_size, new_size) 字节
1:  cmp     r5, r6
    movlo   r2, r5
    movhs   r2, r6                  // r2 = min(old, new)
    mov     r3, #0
2:  cmp     r3, r2
    bhs     3f
    ldrb    r12, [r4, r3]
    strb    r12, [r1, r3]
    add     r3, r3, #1
    b       2b

3:  mov     r0, r1
    pop     {r4, r5, r6, r7, lr}
    bx      lr

4:  mov     r0, #0
    pop     {r4, r5, r6, r7, lr}
    bx      lr