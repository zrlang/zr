#==============================================================================
# runtime/rt_riscv_baremetal.s — zr 运行时（RISC-V 裸机，RV32/RV64）
#
# 与 ARM 版对应：输出走 **semihosting**，内存来自一块静态堆。
# `zr_*` 符号与其它平台保持一致。
#
# psABI 调用约定：a0-a7(x10-x17) 传参、a0 返回；ra(x1) 保存返回地址；
# t0-t6(x5-x7, x28-x31) 为 caller-save 临时；gp(x3)/sp(x2) 由运行时设置。
#
# semihosting 调用序列（按 RISC-V 规范的区分方式，避免被误判成普通 trap）：
#     .option push
#     .option norelax
# 1:  auipc t0, %pcrel_hi(1b)      # t0 = 标签地址
#     addi  t0, t0, %pcrel_lo(1b)
#     li    a7, <操作号>
#     li    a0, <参数>
#     ebreak                        # 仿真器 / 调试器在此接管
# 1:  .option pop
#
# 常用操作号：SYS_WRITEC = 0x11（写一个字符）、SYS_WRITE0 = 0x10
# （写 NUL 结尾字符串）、SYS_EXIT = 0x18（退出）。
#==============================================================================

    .section .text

    .global zr_panic
    .global zr_write
    .global zr_read
    .global zr_exit
    .global zr_alloc
    .global zr_dealloc
    .global zr_realloc

// 寄存器伪名
.equ RA,   1
.equ SP,   2
.equ S0,   8
.equ S1,   9
.equ A0,  10
.equ A7,  17

// 机器字长相关的载入/存储：RV32 用 lw/sw，RV64 用 ld/sd。
// 汇编器会定义 __riscv_xlen（32 或 64）。
#ifdef __riscv_xlen
# if __riscv_xlen == 64
.equ ZR_LD, ld
.equ ZR_SD, sd
# else
.equ ZR_LD, lw
.equ ZR_SD, sw
# endif
#else
.equ ZR_LD, lw
.equ ZR_SD, sw
#endif

//------------------------------------------------------------------------------
// 静态堆：bump allocator 的唯一内存来源（默认 64 KiB）。
//------------------------------------------------------------------------------
    .section .bss
    .align 3
heap_base:
    .zero 65536
heap_end:

    .section .text
    .align 2

//------------------------------------------------------------------------------
// zr_write(fd, buf, count) -> isize
//   a0 = fd, a1 = buf, a2 = count
// 逐字节 semihosting 输出，返回写入字节数。
//------------------------------------------------------------------------------
zr_write:
    mv   t0, a1                  // buf
    mv   t1, a2                  // count
    li   t2, 0                   // 已写入
1:
    beqz t1, 2f
    lbu  a0, 0(t0)
    addi t0, t0, 1
    li   a7, 0x11                // SYS_WRITEC
    ebreak
    addi t2, t2, 1
    addi t1, t1, -1
    j    1b
2:
    mv   a0, t2
    ret

//------------------------------------------------------------------------------
// zr_read(fd, buf, count) -> isize
// 裸机没有输入来源：返回 0（EOF）。
//------------------------------------------------------------------------------
zr_read:
    li   a0, 0
    ret

//------------------------------------------------------------------------------
// zr_seek(fd, offset, whence) -> i64
//   a0 = fd, a1 = offset, a2 = whence
//   whence: 0 = 起点(SEEK_SET), 1 = 当前位置(SEEK_CUR), 2 = 末尾(SEEK_END)
// 裸机没有文件系统：返回 -1（不支持）。
//------------------------------------------------------------------------------
    .global zr_seek
zr_seek:
    li   a0, -1
    ret

//------------------------------------------------------------------------------
// zr_fstat_size(fd) -> i64
//   a0 = fd
// 裸机没有文件系统：返回 -1（不支持）。
//------------------------------------------------------------------------------
    .global zr_fstat_size
zr_fstat_size:
    li   a0, -1
    ret

//------------------------------------------------------------------------------
// zr_isatty(fd) -> i32
//   a0 = fd
// 裸机没有终端：返回 0。
//------------------------------------------------------------------------------
    .global zr_isatty
zr_isatty:
    li   a0, 0
    ret

//------------------------------------------------------------------------------
// zr_exit(code) -> !
//   a0 = code
//------------------------------------------------------------------------------
zr_exit:
    li   a7, 0x18                // SYS_EXIT
    ebreak
1:  j    1b                      // 调试器未接管时自旋

//------------------------------------------------------------------------------
// zr_panic(msg, len) -> !
//   a0 = msg, a1 = len
// 逐字节输出后以 99 退出。
//------------------------------------------------------------------------------
zr_panic:
    mv   t0, a0                  // msg
    mv   t1, a1                  // len
1:
    beqz t1, 2f
    lbu  a0, 0(t0)
    addi t0, t0, 1
    li   a7, 0x11                // SYS_WRITEC
    ebreak
    addi t1, t1, -1
    j    1b
2:
    li   a0, 99
    j    zr_exit

//------------------------------------------------------------------------------
// zr_alloc(size, align) -> *mut void
//   a0 = size, a1 = align
// Bump allocator：对齐到 16 字节，堆耗尽返回 NULL(0)。
//------------------------------------------------------------------------------
zr_alloc:
    la   t0, heap_cursor
    ZR_LD t1, 0(t0)              // cursor
    bnez t1, 1f

    // 首次调用：用静态堆起始地址初始化游标
    la   t1, heap_base
    ZR_SD t1, 0(t0)

1:
    add  t2, t1, a0              // cursor + size
    addi t2, t2, 15
    andi t2, t2, -16             // 对齐到 16
    la   t3, heap_end
    bgtu t2, t3, 2f              // 堆耗尽

    ZR_SD t2, 0(t0)              // cursor = 返回地址
    mv   a0, t1
    ret

2:  li   a0, 0                   // 失败
    ret

//------------------------------------------------------------------------------
// zr_dealloc(ptr, size, align)
// Bump allocator 不归还单块；保留接口完整性。
//------------------------------------------------------------------------------
zr_dealloc:
    ret

//------------------------------------------------------------------------------
// zr_realloc(ptr, old_size, new_size, align) -> *mut void
//   a0 = ptr, a1 = old_size, a2 = new_size, a3 = align
// new_size <= old_size 时原地返回；否则申请新块并拷贝旧内容。
//------------------------------------------------------------------------------
// 调用 zr_alloc 会破坏 t0-t3，因此跨越调用的值放在 callee-save 的
// s0/s1（并在栈上保存，因为本函数自己也是被调用者）。
zr_realloc:
    addi sp, sp, -32
    ZR_SD s0, 24(sp)
    ZR_SD s1, 16(sp)
    mv   s0, a0                  // old ptr
    mv   s1, a1                  // old size
    mv   t0, a2                  // new size
    mv   t1, a3                  // align

    bgeu t0, s1, 1f              // new_size <= old_size：原地返回
    mv   a0, t0                  // size = new_size
    mv   a1, t1                  // align
    call zr_alloc
    beqz a0, 2f                  // 分配失败

    mv   t2, a0                  // new ptr
    mv   t3, s1                  // min(old_size, new_size)
    bgeu t0, s1, 3f
    mv   t3, t0
3:
    li   t4, 0
4:
    bgeu t4, t3, 5f
    lbu  t5, 0(s0)
    sb   t5, 0(t2)
    addi s0, s0, 1
    addi t2, t2, 1
    addi t4, t4, 1
    j    4b
5:
    mv   a0, t2
    j    6f
1:
    mv   a0, s0
    j    6f
2:
    li   a0, 0
6:
    ZR_LD s1, 16(sp)
    ZR_LD s0, 24(sp)
    addi sp, sp, 32
    ret

//------------------------------------------------------------------------------
// 堆游标（与静态堆同属 .bss，供 zr_alloc 使用）。
//------------------------------------------------------------------------------
    .section .bss
    .align 3
heap_cursor:
#ifdef __riscv_xlen
# if __riscv_xlen == 64
    .zero 8
# else
    .zero 4
# endif
#else
    .zero 4
#endif