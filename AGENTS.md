# AGENTS.md — zr 编译器贡献指南

zr 是一门系统编程语言（Rust/Zig 混合取向），编译器用 Rust 写、**零第三方依赖**。
本文件给在代码库里工作的 AI agent 看，是操作约定与"哪些文档可信"的清单。
项目入口与功能全景见根目录 `AGENTS.md`。

> 最近更新：2026-11-02。工作树在 `feat/lang-features`；调用实参 / 字段值的类型
> 核验已落地（`check_call_arg` / `check_field_value`），`std/error` 因此可加载；
> `yield` 已从类型占位升级为可执行的协作让出点（非生成器）；trait 默认方法
> （带方法体的 trait 方法 / `default fn` 拼写）已合成到 impl 目标类型；
> 带捕获闭包已能转成裸 `fn(..)` 值（函数值 = 16 字节记录 `{code, env}` + 统一
> shim ABI，P1-2c）。清单见 §5。

---

## 0. 先读这个：文档可信度分级

| 文档 | 可信度 | 说明 |
|---|---|---|
| **源码注释** | ✅ 最高 | 注释很密，且常记录"早先怎么错、为什么这么改"，是排查资料的第一手来源 |
| `AGENTS.md` / `AGENTS.md` | ✅ 高 | 已逐条核对源码 |
| `README.md` 的**语法与用例** | ✅ 高 | 跟着用例就能写出能跑的程序 |
| `README.md` 的**安全模型**（"指针与 unsafe"） | ⚠️ 愿景 | 描述**不是**现状（借用检查/生命周期未实现）；2026-10-29 已加警示注明是设计目标 |
| `docs/rust-parity.md` | ✅ 高 | 2026-10-29 已修正（泛型别名改"已支持"、`dyn` 改"骨架可用"）；仍以源码为准 |
| `CONTRIBUTING.md` | ✅ 高 | 2026-10-29 已补测试约定与架构红线摘要 |
| `tests/README.md` | ✅ 高 | 2026-10-29 已重写"尚未覆盖"表并补 52–90 用例清单 |
| `future/README.md` | ✅ 高 | 2026-10-29 已按源码实际结论重排状态列 |
| `Cargo.toml` description | ✅ 高 | 2026-10-29 已改为 "ownership, zero-cost abstractions, and coroutines" |

**结论：以源码为准。任何"已实现"的声明都要 `grep` 验证。**

`future/README.md` 的状态列在 2026-11-01 已按源码重排，当前的不接线 / 空壳项：
`lto`（`src/opt/lto.rs` 有完整实现，但 `opt/mod.rs` 未声明、CLI 无开关）、
`pgo`（`src/opt/pgo.rs`，同上）、`macro_system`（`src/macro_system/`，`main.rs` 未 `mod` 声明；
`macro_rules!` 走的是 `src/macros.rs`）、`impl_trait` 的**返回位置（RPIT）**仍有限：
`-> impl Trait` 是真存在类型 `opaque(impl_trait_N)`，隐藏类型记进 `opaque_hidden`，
方法查找 / 字段访问 / 边界校验都按隐藏类型走（`167_rpit_field.zr`，负向 `47`/`48`），
但 `impl Fn(T) -> T`（无 `Fn` trait 语法）与 trait 方法签名里的 RPIT 仍不支持。
**已接线且确实可用**：`const_generics`（`<const N: usize>` 全链路，长度表达式
`[T; N + 1]` 也已支持，见 §5 的在途工作）、`coherence_rules`（`check_orphan_rule` /
`check_overlap`，`src/typeck/mod.rs:1812-1814`）、`c_backend`（`--emit-c`）、
`specialization` 的**选择**（更 specific 的 impl 优先，但无 `default fn` 特化）、
`negative_bounds`（`where T: !Trait`，`168_negative_bounds.zr`，负向 `38`）、
`higher_ranked`（`for<'a> T: Trait<'a>`，`169_hrtb.zr`）、`zr0`（`zr0-check` /
`zr0-c` 子命令已接线，`main.rs` `mod zr0`）。

---

## 1. 构建 / 测试 / 诊断

```sh
cargo build                            # 也构建 runtime/*.s → libzrrt-<平台>.a
cargo build -q 2>&1 | tail -20         # 只看错误
cargo test --test zr_language          # 全量回归，约 2.5–3 分钟
target/debug/zr prog.zr -O1 -o /tmp/prog   # 编译单个程序
```

- **运行时链接失败**（`requires the platform linker` + `runtime source ... was not built`）
  有时是环境问题（重跑 `cargo build`），**但别急着归因**——2026-10-29 在
  `60_process` / `64_fs_path` / `74_generic_alias` 上发现同样的报错其实是
  **真实 bug**：用户 `fn read` 与 prelude 拉进来的 `core` 模块 `pub fn read`
  同名，MIR 降级时都加 `z_` 前缀发射，目标文件里 `_z_read` 定义两次，macOS
  `ld` 直接断言崩溃（`atomShouldReplaceExisting`）。先 `nm` 看目标文件有没有
  重复符号，再下结论。已修复（提交 `1ef4a02`：MIR 按最终符号名去重，保留首个
  用户定义）。
- 测试：`tests/zr_language.rs` 把 160 个 `tests/cases/*.zr` 各在
  `-O0/-O1/-O2/-O3/-Oz` 跑一遍（800 次编译+运行）。退出码 0 = 过，非 0 = 失败（编号指向失败的断言）。
  宿主端另有 `io_print_matches_rust` 等 Rust 对照用例。
- **负向测试机制是 `tests/zr_negative.rs`**（`cargo test --test zr_negative`，
  46 个用例）。`tests/negative/*.zr` 每个都必须**编译失败**，且报错文本里含
  文件里 `// expect: <子串>` 标注的期望子串；没有 `// expect:` 的行只要求
  "确实报错"。子串而非精确匹配，是为了不让行号/列号漂移把整个套件变红。
  `tests/cases/` 下每个 `.zr` 仍然必须编译通过并返回 0，**两边不要混放**
  （`zr_language_cases` 用 `read_dir` 扫 `tests/cases`，放进去就会被当成编译失败）。
- 最小复现优先放 `/tmp/`，**别塞进 `tests/cases/`**（塞进去就会挂测试）。

诊断开关：`--dump-tokens|ast|hir|mir|ssa|ssa-after|asm`、`--wat`、`--emit-llvm`；
环境变量 `ZR_TIME=1`（各阶段耗时）、`ZR_VERBOSE=1`、`ZR_DEBUG_ALLOC`、`ZR_DEBUG_BACKEDGE`。
`cargo test` 输出很长，重定向到文件再 grep。

---

## 2. 流水线与模块地图

```
lexer → parser(ast) → typeck(hir, infer) → mir → ssa → opt → codegen → ELF/Mach-O/PE/wasm
```

`src/main.rs:19-35` 的 `mod` 声明就是**已接线清单**。

- **类型检查主战场是 `src/typeck/mod.rs`（约 8100 行），不是 `infer.rs`。**
  impl 匹配的真实实现在 `mod.rs::impl_target_matches`；`infer.rs` 里的
  `impl_matches_type` 是恒返回 `true` 的死桩，别去那里找逻辑。
- 后端 6 个：`encoder.rs`(x86-64)、`encoder_aarch64.rs`、`encoder_arm.rs`(A32)、
  `encoder_thumb.rs`(Thumb-2)、`encoder_riscv.rs`(RV32/64IM)、`wasm.rs`(wasm32/64)。
  目标格式：`objfile.rs`(ELF)、`macho.rs`、`pe.rs`、`wasm.rs`。另有 `c_backend.rs`：**已接 CLI**
  （`--emit-c` 从 HIR 直译成一个 C 翻译单元，自举路径，见 `main.rs:762-766`）。
- 标准库 `std/`：**三层架构（core ⊂ alloc ⊂ std，与 Rust 对应）**，共 32 个 `.zr` 源文件
  （编译器自编译）：
  - `std/core/`（零依赖：无堆、无 OS、无 I/O）——`core` `simd`
  - `std/alloc/`（只依赖 core，带堆分配）——`mem` `slice` `string` `collections`
    `json` `xml` `toml` `base64` `md5` `sha256` `crypto` `tensor` `pdb` `async`
  - `std/std/`（依赖 OS）——`sys` `io` `fs` `net` `http` `http2` `tls` `env` `os` `posix`
    `process` `thread` `time` `tz` `path` `argparse` `error`
  - `std/std.zr`：**门面**，`use core::*; use string::*; …` 把三层名字传递性再导出，
    所以业务代码可以 `use std::*` 或 `use std::io::File`。
  **模块名仍是扁平的**：`use core::Option;` / `use io::File;` 照旧可用；
  `use std::core::Option;` 也行（走门面）。
  层级由**文件所在目录**决定，依赖方向越界（core→alloc/std、alloc→std）在加载阶段
  直接报 `layering violation`（`src/modules.rs::layer_of_path` + `load_uses`）。
  查找顺序：`std/<name>.zr`（门面/历史）→ `std/core/` → `std/alloc/` → `std/std/`。
  **2026-11-03 实测**（macOS x86-64，`use <m>::*;` + 空 `main`，全链路编译+链接）：
  **30 个通过**（除 `async`、`crypto`、`tls` 外全部；`crypto` / `tls` 不依赖外部链接但主程序未引入时不参与自链接，`-c` 单独验证 OK；TLS 是纯 zr 实现、零外部依赖）。新加的 `argparse` / `base64` / `crypto` / `md5` / `sha256` /
  `time` / `tz` / `os` / `posix` / `http` / `http2` / `pdb` / `simd` / `tensor` / `tls` 均有正向用例
  126/131–152 覆盖；`cli` 已重命名为 `argparse`）。
  另有 `error` 因类型核验落地而可加载——`as *const _` 推断占位、字面量传 `push_bytes`
  的切片放行均已补上；`(&dyn Error).cause()` 的 vtable 分派已可用（x86-64，
  见第 4 节 dyn 条）。`async` 仍不可用——`Box` 类型未实现 + `*mut dyn Wake` 需要 vtable
  分派（结构体 `where` 子句、`F::Output` 关联类型投影已在本分支补上，不再是阻塞项），
  因此它暂不并入 `std` 门面。
- 运行时 `runtime/*.s`：每平台一份汇编（Linux/macOS/Windows 的 x86-64+aarch64，裸机 ARM/RISC-V）。
  提供 `zr_write` / `zr_alloc` 等符号，与编译器分离编译。
- 已接线的优化 pass（`src/opt/mod.rs:68-102`，按 `-O` 等级分层）：
  `fold_constants` `eliminate_dead_code` `eliminate_common_subexpr` `global_value_numbering`
  `eliminate_dead_blocks` `sparse_conditional_constant_propagation` `peephole`
  `inline_functions` `optimize_loops`。
  **`inline_functions` 只内联单块函数**（`#[inline(always)]` 的多块函数也跳过），
  重命名必须一次性映射全部操作数——见第 5 节第 1 条。

### 确认是死代码 / 未接线的

| 路径 | 状态 |
|---|---|
| `src/zr0/` | `main.rs` 未 `mod` 声明 |
| `src/macro_system/` | `main.rs` 未 `mod` 声明（`macro_rules!` 走的是 `src/macros.rs`） |
| `src/opt/lto.rs` | `opt/mod.rs` 未声明。**P2-2a 已评估：不接线**——单模块流水线下 `run_lto` 只收一个 `SsaModule`，跨模块内联无对象可并；留待 v0.13 重评 |
| `src/opt/pgo.rs` | `opt/mod.rs` 未声明。**P2-2a 已评估：不接线**——PGO 需要插桩 + profile 落盘 + 二次编译的完整闭环，CLI 连 `-C` 开关都没有；留待 v0.13 重评 |

**不要在这几个目录里"修 bug"——先确认它们该不该接线。**
（`src/codegen/c_backend.rs` 已由 `--emit-c` 接线，不属于此表。）

---

## 3. 架构红线（务必先读）

1. **编译跑在 256 MiB 栈的线程上**（`main.rs:170-173`）。栈溢出 = **abort**，
   不可恢复、无诊断输出。写递归解析/检查代码必须能证明终止，别靠"应该不会爆"。
2. **`Ty::Infer(InferId(0))` 不是哨兵，是活的推断变量。** 大文件里会被绑定掉。
   拿它当"未知类型"的占位返回值会让 `subtype` resolve 出旧值，报出完全无中生有的错
   （曾导致 `std/std/io.zr` 报 `expected (), found u8` 且报错位置与真实问题无关）。
   **不一致时应返回 `Unit` 或显式报错，绝不复用已有 `InferId`。**
   同理，**不要把 `infer.next_infer` 归零当"重置"**：声明阶段（`record_type_generics`
   等）创建的占位推断变量仍活在 `type_generic_params` 里，归零会让主循环的
   `fresh_infer` 与旧占位符撞号（`66_generics.zr` 的 `expected i32, found Pair<i32>`）。
   重置只需 `vars.clear()`，别动发号器（见 `check_function_bodies` 的两遍检查）。
3. **`tail.is_none()` ≠ 发散。** 两种情况 tail 都是 `None`：块以 `return;` 结尾（真发散），
   或块最后一句带了分号（`f(x);` 后直接 `}`，不产生值但**不**发散，类型是 `()`）。
   混同会让 `if` 分支被误判成 `Never`。发散要按 AST 形状判（`block_diverges` / `expr_diverges`）。
4. **`type_mismatch(expected, found)` 的参数顺序在 `unify` 和 `subtype` 里相反。**
   `unify(a, b)`：`a` 期望、`b` 实际。`subtype(sub, sup)`：`sub` 实际、`sup` 期望。
   写反了报错文案整段倒过来（`expected u8, found ()` 其实是 `expected (), found u8`）。
5. **`subtype` 的错误被 `.ok()` 静默吞掉**（`check_block` 里），一个标准库文件里能攒出
   70 多个 `Int(32)`/`Uint(64)` 宽度不匹配。**编译能通过不代表没有类型不一致。**
6. **`#[derive]` 生成的 impl 块 `trait_id` 是 `None`**（属性 args 只有字符串）。
   求解必须靠 `trait_defs_by_name` 反查名字；只认**真正写在 `derive` 属性里**的名字，
   不能一见 `derive` 属性就算满足——那样 derive 能满足任何 trait。
7. **占位降级（下游会拿到错的值）**：`src/ssa/mod.rs:1301` `Deref => SsaUnaryOp::Not`；
   `src/mir/builder.rs:1358` `AddrOf => MirUnaryOp::Deref`。
8. **`--emit-llvm` 输出的是手写伪 IR 文本**（整数统一 i64、指针 i8*），`clang` 消费不了。
   已标**实验性**（P2-3a）：help 文案写明 "pseudo LLVM IR (experimental)"，发射时
   stderr 打警告；`toolchain_subcommands_emit` 断言警告存在。
9. **内联时的值重命名必须"同时替换"**，不能按值逐个 `replace_operand`：被内联体
   `%10 = %1 Add %9` 里先把 `%1` 换成新号 `%9`，再处理 `%9` 就会把两个操作数一起换掉
   （`spawn` 的帧初始化把偏移量当指针，`-O3` SIGSEGV）。用 `map_operands`。
10. **REX 的位是 W=0x08 / R=0x04 / X=0x02 / B=0x01，`0x09` 不是"R|B"而是"W|B"。**
   `xor reg, reg`（`31 /r`）的 dst 同时占 reg 与 r/m 两个字段，r8–r15 要置
   `REX.W|R|B = 0x4D`；写成 `0x48 | 0x09` 会丢 REX.R，编码成 `xor rax, r8`
   ——零常量就变成"用 rax 异或"。新增 REX 计算时先写出位定义再拼。
11. **后端的"这个值是不是浮点"是**反推**出来的**（`assign_float_slots` 扫
   `SsaInstr::Binary { ty: Float }` 等指令），形参本身**没有类型标注**。所以
   形参必须靠 `SsaFunction::param_types`（从 MIR 抄来）**显式播种**：只被转手的
   浮点形参（`fn outer(f: f64) { inner(f) }`）反推不出来，会被当成整数走 rdi，
   而调用方按 SysV 放在 xmm0 —— 位模式当指针用直接 SIGSEGV，或者后面所有整数
   形参整体错位（读到的全是别人的值）。新增后端要照抄这一步。
12. **`SsaInstr::Call` 不携带实参类型，实参宽度只能查被调方的声明。**
   128 位实参要占**两个**实参寄存器，少送一个字会让被调方读到下一个实参
   （`div128(-100, 7)` 只送 rdi:rsi 而 `_z_div128` 读 rdi:rsi:rdx:rcx，-O2/-Oz
   直接 SIGILL）。宽度靠 `regalloc::CalleeParams`（`codegen::compile` 用
   `module.functions` 的 `name`/`param_types` 建表，逐层传给
   `emit_one` → `emit_function` → `assign_int_widths`）判定；`CallIndirect` 的
   被调方是运行期指针、没有声明可查，故**不标**实参宽度——间接调用目前只能
   传机器字宽的值。新增后端必须接这个参数，别只按返回值标宽。
13. **`HirStmt::Let` 只承载一个 `local`，解构模式必须展开成多条语句。**
    `Statement::Let` 的简单绑定（`HirPattern::Binding`）直接落成一条 `Let`，
    其余模式（元组 / 结构体 / 切片 / 通配）要先"临时 scrutinee + 逐子值
    `Let`"展开（复用 `pat_match`）。只取 `first_binding(...)` 当唯一绑定会让
    子绑定**永不初始化**——`let (a, b) = (3, 4); a + b` 编译通过、读到栈上垃圾。
    `HirParam` 同样只有一个 local，函数形参上的解构模式仍是这个限制。
14. **vtable 槽宽 `VT_ENTRY_BYTES`（`src/mir/builder.rs`）必须与 x86-64 后端的
   vtable 发射（`src/codegen/mod.rs::emit_vtables_x86`）逐字节同步。**
   vtable 是 `.text` 跳转表：每槽位定长 `jmp rel32`（E9，5 字节），
   MIR 侧按 `表址 + index × VT_ENTRY_BYTES` 算槽址、codegen 侧按同样宽度发射。
   现有重定位全是指令级 PC 相对（无 64 位绝对项），所以**不要**把 vtable 改成
   `.data` 函数指针数组——那需要先引入 `R_X86_64_64` 之类的重定位种类。
   槽宽改了只改一侧：跳错地址，运行期 SIGSEGV 且难以定位。

---

## 4. 安全模型的现实

README 里"指针与 unsafe"一节目前是**愿景**：

- **`unsafe` 是关键字、`unsafe { ... }` 能解析并通过类型检查，但不改变语义**
  （`src/lexer/mod.rs:815` `KwUnsafe`、`src/parser/expr.rs:449`；按块表达式透传）。
- **`dyn Trait` vtable 分派已落地（x86-64 MVP，2026-11-04）**：`Ty::Dyn` 是
  16 字节**值类型** `{ data@0, vtable@8 }`（与 `str` / 切片 / 函数值同布局惯例），
  `&dyn Trait` 是指向该记录的 8 字节指针。`let d: dyn Tr = &x;` 由
  `coerce_to_dyn` 触发：源类型须实现 `Tr`（未实现报
  `type mismatch: expected `dyn Tr``，负向 `49`）；`&x` 产生的 `*const` 裸指针
  也放行。vtable 符号 `__vt_<Type>_<Trait>`，槽位顺序 = trait 方法声明顺序
  （cfg 掉的不占槽位），默认方法按类型合成进表。调用经 `ExprKind::DynCall`
  降级为间接调用（见红线 14 的槽宽同步约束）；聚合返回（`String` / `str` /
  枚举）与 `CallIndirect` 同约定，实参末尾追加 sret 槽。
  **MVP 边界**：仅 x86-64 后端（其它架构在 `compile` 入口明确报错）；目标类型限
  非泛型具名类型 / 基元；方法签名除 receiver 外含 `Self` 的不可 dyn 调用；
  vtable 无 drop / size / align 元数据（`Box<dyn Trait>` 析构仍缺）。
  正向 `161_dyn_dispatch.zr` / `162_dyn_std_error.zr`（`std/error.zr` 的
  `description() / cause() / backtrace()` 经 dyn 分派可用）；负向 `49` / `50`。
- **切片/数组索引不做越界检查**：`check_index_scalar` 不检查边界。
- **借用检查是最小切片**：同位置的借用与已有借用重叠（`&mut` 独占、`&` 可共享，
  至少一个可变）报 `cannot borrow ... while already borrowed`；**写被借用位置**
  （`x = 5` 而 `&x` / `&mut x` 存活）报 `cannot assign to ... while it is borrowed`；
  **读被借用位置**（`use x` 而 `&mut x` 存活）报 `cannot read ... while it is
  mutably borrowed`（字段级不相交 `&mut x.a` 后读/写 `x.b` 合法，由
  `place_conflicts` 前缀判据处理）。
  **NLL 近似（last-use）**：`let r = &mut x` 的借用绑定到 `r`，存活到 `r` 的
  **最后使用**（不是作用域结束）——`let r = &mut x; *r = 5; if x != 5`（读 `x`
  时 `r` 已死）、`let r = &mut x; *r = 5; let s = &mut x;`（再借 `x` 时 `r` 已
  死）都合法；读/写/再借在 `r` 的最后使用**之后**不报冲突。整个函数体检查完、
  最后使用已知后由 `finish_borrow_accesses` 统一判定。**词法作用域近似**：临时
  借用（`foo(&mut x)`）在语句结束释放，`&mut x` 强转裸指针（字段类型 `*mut` 或
  `as *mut T`）立即释放，`let` 初始化里的**调用实参**（`let v = early(&mut x)`）
  是临时量不是绑定值（`with_value_pos`），语句结束释放。
  已知局限：多绑定 / 未关联的借用（`let (a, b) = (&mut x, &mut y)`）仍按作用域
  结束；`binding_last_use` 按局部 id 记最后使用，跨作用域重名（遮蔽）可能混淆。
- **移动语义只做最小检查**：`let b = a` 后读 `a` 报 `use of moved value`，
  但**仅当 `a` 的类型含 drop glue**（自身或成员递归 `impl Drop`，
  `has_drop_glue`，`src/typeck/mod.rs:5895`）。`std/alloc/string.zr` 的 `String`
  与 `collections` 的 `Vec` **都没有 `impl Drop`**，因此 `let s2 = s1; s1.len()`
  仍静默通过（`std/std/net.zr:752` 的 `Socket` 是唯一带 Drop 的标准库类型）。
  字段级移动、实参移动、容器内元素移动都不检查。
- **可见性检查**：解析阶段保留 `pub` / `pub(crate)` / `pub(super)`（早先
  `_visibility` 直接丢弃，`pub(crate)` / `pub(super)` 被当成 `pub`，私有条目
  跨模块访问不设防）。模块加载器给每个条目打上声明模块路径，类型检查在
  `resolve_name` / `resolve_path` 里按"条目可见性 × 引用方模块"判定：`pub` /
  `pub(crate)` 单 crate 内处处可见（单 crate 编译，两者等价）；`pub(super)`
  对父模块及其后代可见（顶层模块的父模块 = crate root）；私有 / `pub(self)`
  对声明模块及其后代可见。`extern "C"` 声明按 `pub` 处理（同一 `zr_write`
  可能在多个模块的 extern 块里重复声明，扁平符号表只留先收集者）。
  已知局限：`use m::item` 导入私有条目的检查在 `use` 点未做（内联模块的
  `use` 本就不支持），`resolve_name` 的 `use_aliases` 分支绕过可见性检查。
- **`str` 是 `{ptr, len}` 结构体**，切片写成 `[]const T`（元素在后），不是 Rust 的 `&[T]`。
- **`println` 是函数不是宏**：`println(s: str)`；格式化走 `println_fmt(fmt, &[FormatArg], n)`，
  支持 `{}` `{:?}` `{:x}` `{:X}` `{:b}` `{:o}` `{:e}` `{:E}`、位置参数、填充/对齐/`+`/`#`/`0`/宽度/精度、
  动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`（与 Rust 逐字符对比一致）；
  **命名参数 `{name}`** 走 `println_fmt_named(fmt, &[NamedArg], n)`（名字与值放
  同一个结构体）。**自定义类型的 `{:?}` / `{:#?}`** 走**类型化值通道**：
  `FormatArg` 加 `kind = 7`（custom）+ `obj_ptr: *const void` + `debug_fn`
  函数指针，调用方写 `io::fmt_debug(&v as *const void, Type__debug_fmt)`
  （`Type__debug_fmt` 是 `#[derive(Debug)]` 生成的
  `fn(*const void, bool, *mut u8, usize) -> usize`），`#` 标志在**运行期**
  决定走 `.debug()`（单行）还是 `.debug_pretty()`（多行）。同一类型两种形态
  逐字节对照 Rust（`tests/cases/112_fmt_debug.zr`、`io_print_matches_rust`）。
  `{:#?}` 对**标量**仍只输出一行（`#` 对标量被忽略）。
- **`Path {}`（零字段）结构体字面量可解析**（`41d8cb9`：`{` 后直接是 `}` 也算
  结构体字面量，见 `looks_like_struct_literal`）。
- **索引不做越界检查**，但对**标量**下标会在类型检查阶段报错
  （``cannot index a value of type `u8` ``）；指针下标写成 `p[i]`，不要写 `(*p)[i]`。

---

## 5. 最近修掉的静默 bug 与当前基线

2026-10-27 的 `feat/lang-features` 上连续几个提交修的都是"编译通过、运行不崩、
结果全错"的静默 bug；2026-10-29 又修掉了"用户定义被标准库同名函数遮蔽"的两个
关联 bug（提交 `1ef4a02`，详见 §1 的链接失败说明）：

| 文件 | 在做什么 |
|---|---|
| `src/typeck/mod.rs` | `alloc_def_top`（同名保留用户）+ `function_def_ids`（阶段 6/7 逐条取本函数 DefId）：用户 `fn swap` 不再被 prelude 拉进来的 `core` 的 `swap<T>` 覆盖，元组返回类型不再被静默改成 `()` |
| `src/parser/decl.rs`、`src/modules.rs`、`src/typeck/mod.rs` | `use foo::*` glob 导入（提交 `40b5b45`）：解析器保留 `*` 前的路径段（早先 `use core::*` 把 `core` 丢掉）；模块加载器记录每个模块的导出名（`module_exports`），类型检查的 Glob 分支把它们全部登记成别名 |
| `src/mir/mod.rs` | 按最终符号名去重（保留首个用户定义）：用户 `fn read` 与 `core` 的 `pub fn read` 同名同体不再都发射成 `z_read`，`_z_read` 不再定义两次，macOS `ld` 不再断言崩溃 |
| `src/ssa/mod.rs` | 支配边界从"直接前驱版"改成 Cooper–Harvey–Kennedy：漏 φ 会让合流块读到未初始化的栈槽 |
| `src/opt/pass_manager.rs` | 内联重命名改成"先建完整重命名表、再逐操作数一次性映射"（见下） |
| `src/codegen/encoder.rs` | SysV 栈上传参（第 7 个整数 / 第 9 个浮点实参）+ `call` 的 16 字节对齐；零常量的 `xor reg, reg` 补回 REX.R |
| `src/typeck/mod.rs` | `check_index_scalar` 不再吞 unify 错误；`check_match` 给穷尽 match 补发散 else；聚合类型上的 `==`/`<` 改写成 derive 方法调用 |
| `src/typeck/mod.rs` | 枚举缺臂报错列出**缺失变体名**（提交 `a526953`）：早先统一说 `match must have a trailing `_` arm`，误导人以为补个 `_` 就行；现在 `match is not exhaustive: `Color` variant not covered: Blue`，整型/标量仍保留尾 `_` 臂措辞 |
| `src/typeck/derive.rs` | 新增：在 AST 层生成 `Type__clone` / `__eq` / `__lt` / `__hash` / `__default` 方法体 |
| `src/typeck/hir.rs`、`src/mir/builder.rs` | 新增 `ExprKind::Unreachable` → `MirTerminator::Unreachable`（上面的发散 else） |
| `src/typeck/infer.rs` | 诊断名字表：`unify`/`subtype` 的错误不再把具名类型打成 `type#N` |
| `std/std/io.zr` | `(*p)[i]` → `p[i]`（两处）；`{:#X}` 的前缀改回小写 `0x`（与 Rust 一致） |
| `src/ssa/mod.rs`、`src/codegen/encoder*.rs` | `SsaFunction` 新增 `param_types`（从 MIR 抄），两个后端拿它给浮点形参**播种**：只被转手的 `f64` 形参早先被判成整数，走 rdi 而调用方放 xmm0 |
| `std/std/io.zr` | 格式串补 `{:e}` / `{:E}`（浮点与整数都走十进制指数形式，整数按有效数字四舍五入）、动态宽度 `{:1$}`、动态精度 `{:.*}` / `{:.1$}`；顺带修 `{:.}`（点后面没数字）不再被当成精度 0 |
| `src/mir/builder.rs` | `const X: str = "..."` 引用：常量值表里存的是 `MirValue::Str`（后端发射成 8 字节字符串指针），早先直接当表达式值返回，写进 16 字节 `str` 局部只填了 ptr、`len` 留在栈上垃圾（`const LAST: str = "@last"; LAST.len` 实测读出 `0x666e69004e614e`）。现在聚合形态的字符串常量与字符串字面量走同一套"栈上架 `{ptr, len}`"的降级。钉子：`tests/cases/130_toml.zr` 用常量当 `get` / `get_path` 的键 |
| `src/mir/builder.rs` | **聚合字面量里的移动没有登记**（2026-11-03）：`TcpStream { inner: sock }` 只把 `sock` 的字节拷进新聚合，却不标记源已移动，源局部的 drop glue 仍在作用域结束时发射——`TcpStream::connect` 返回时 `sock` 被 drop、fd 关掉，随后 `stream.write` 一律 EBADF（`std/net.zr` 的 `TcpStream` 之前根本用不了；`std/http.zr` 走裸 fd 才没暴露）。现在 `Struct` / `Tuple` / `EnumVariant` 三个字面量分支在降级字段前调 `mark_aggregate_move`（复用 `mark_place_moved` 的路径跟踪）。钉子在 `std/std/http2.zr` 的 h2c 往返（人工探针） |
| `tests/cases/76..79`、`81`、`tests/negative/12..15` | 分别钉住上面每一处 |

### 三个"改对了才算改对"的点（代码里都有长注释）

1. **内联重命名必须同时替换**：被内联函数体 `%10 = %1 Add %9`，如果按值逐个
   `replace_operand(instr, from, to)`——先把 `%1` 换成调用点的新号 `%9`，指令变成
   `%10 = %9 Add %9`，接着处理 `%9` 就把两个操作数一起换掉了。症状：`spawn` /
   future 的帧初始化把偏移量当指针，往 `0x20` 写，`-O3` 直接 SIGSEGV（`-O0/-O2`
   不内联所以看不出来）。现在用 `map_operands` 一次性映射。
2. **φ 位置要按迭代替换支配边界求**：`DF(x) = { y | x 支配 y 的某个前驱，x 不严格支配 y }`。
   只算 `x ∈ preds(y)` 会漏掉"if 里套循环"的汇合块，走 else 路径时读到上一轮的垃圾。
3. **栈上传参的偏移要跟着 `rsp` 走**：`Src::Mem(slot)` 是 `[rsp + 8*slot]` 的原始帧偏移，
   压入实参后必须整体加 `released/8`；被调方读 `[rbp + 16 + 8k]`（`rbp` 才是稳定基准）。

### `cargo test` 当前基线（2026-11-03，全绿）

- `cargo test --bin zr`：269 项（含标准库三层重组的 2 个 `layer_tests`）。
- `cargo test --test zr_language`：160 个用例 × 5 个优化等级 = 800 项，另有 3 个宿主端
  对照用例（`io_print_matches_rust`——逐字符对比 Rust 的格式化输出，含 `{:e}` / `{:E}`
  与大浮点 Display（`|v| ≥ 2⁶⁴` 不带 `.0`）的回归；`extern_c_write_prints_string_literal`；
  `toolchain_subcommands_emit`——`zr fmt` / `zr fmt --check` / `zr pkg init` / `zr lsp`
  与 `--emit-llvm` / `--wat` 的发射验证）。
- `cargo test --test zr_negative`：37 个负向用例（含枚举缺臂
  `17_match_missing_variant.zr`、`19_debug_float.zr`、借用检查 20/21/22、
  带显式判别式的枚举 Ord `26_enum_ord_discriminant.zr`、调用实参核验
  `27_scalar_to_ref_arg.zr` / `28_str_to_ref_string.zr`、限定路径的 trait 校验
  `29_qualified_trait_not_impl.zr`、APIT 边界校验
  `30_apit_bound.zr`、`let` 位置的可反驳模式
  `31_let_refutable.zr`、const 泛型实参种类 / 值不匹配
  `32_const_arg_kind.zr` / `33_const_param_as_type.zr` / `34_const_arg_mismatch.zr`、
  同步函数与闭包体里的 `yield` 上下文校验
  `35_yield_sync.zr` / `36_yield_closure.zr`、impl 缺必需 trait 方法
  `37_trait_missing_method.zr`、泛型函数 / 异步函数当值
  `38_generic_fn_as_value.zr` / `39_async_fn_as_value.zr`）。

### 最近修掉的静默 bug（都在 `tests/` 里留了钉子）

- **`let` 解构模式子绑定永不初始化**：`Statement::Let` 算完 `hir_pat` 后只取
  `first_binding(...)` 当唯一绑定，而 `first_binding` 只认 `HirPattern::Binding`——
  元组 / 结构体 / 切片模式的子绑定由 `lower_pattern` 分配却永不写值：
  `let (a, b) = (3, 4); return a + b;` 编译通过、exit 192（应 7）；
  `let [a, b] = arr;` exit 208（应 7）。现在非简单绑定时展开成
  "临时 scrutinee + 逐子值 `HirStmt::Let`"（复用 `pat_match`），可反驳模式在
  `let` 位置报 `refutable pattern in local binding; use \`let ... else\``。
  切片模式顺带从"明确未实现"变成可用：`HirPattern::Slice` 携带子模式，
  `pat_match` 按定长数组的编译期长度 / 切片的运行期 `.len` 生成条件，
  绑定落在 `scrut[i]` 上。钉子：`tests/cases/121_let_destructure.zr`、
  负向 `tests/negative/31_let_refutable.zr`。
- **穷尽 `match` 的返回值被 `Unit` 覆盖**：靠变体/bool 全覆盖证明穷尽的 match 没有
  `_` 臂，脱糖出的 if/else 链最后一条分支"有值"，于是整块被当成 `()`，返回值被
  丢掉。现在 `check_match` 在这种形状下补一个发散 else（`hir::ExprKind::Unreachable`
  → `MirTerminator::Unreachable`）。
- **零常量没有清零自己的寄存器**：`emit_const_int(_, 0)` 走 `xor reg, reg` 捷径，
  REX 只置了 0x09（W|B，W 本就在 0x48 里），REX.R 丢了 —— r8–r15 被编码成
  `xor rax, r8`，零常量变成"用 rax 异或"。症状是 `match` 里返回常量 `false`
  的路径给出 32513 之类的垃圾。钉子：`tests/cases/79_zero_const_high_reg.zr`。

### 在途工作（`feat/lang-features` 上）

以下项已在本分支上完成或部分完成，更新如下：

- **生命周期参数 / 实参（`Foo<'a>` / `&'a mut T` / `&'static dyn T`）**：✅ 已可解析并
  全链路跑通（2026-11-01）。三处解析缺口：(1) `parse_generic_args` 早先对每个实参都
  调 `parse_type()`，`Foo<'a>` 里 `'a` 被前缀吃掉后撞上 `>`，报
  `expected type, found Gt`——`impl<'a> Ctx<'a>` 的头解析不了；(2) `&` 分支先看
  `mut`，`&'a mut T` 的 `'a` 被当成"不可变"，`mut` 撞 `expected type, found KwMut`；
  (3) 跳生命周期的代码排在 `dyn` 分支之后，`&'static dyn Error` 报
  `expected type, found KwDyn`。现在生命周期实参**跳过不登记**（`Type` 没有生命周期
  变体，类型检查本来就整体丢弃），`&` 后先吃生命周期，跳过提到前缀最前面。
  **下游还有一处**：`check_function_bodies` 早先看 `impl_gens.is_empty()` 判"泛型 impl"，
  纯生命周期 impl 的方法体被整体跳过 → 方法永不发射、调用点留未定义符号
  （`_Ctx__get`），链接期才炸。现在只把**类型 / const** 参数算泛型。
  回归 `tests/cases/115_lifetime_args.zr`。**仍不支持**：`'_` 作为**必须**的实参
  （会被跳过，`Foo<'_>` 等于 `Foo`）；结构体 `where` 子句与 `as *const _` 已补上。
- **泛型关联类型投影 `F::Output`**：✅ 已实现——早先 `F::Output`（`F: Future`）报
  `cannot find value F`：`ast_to_hir_type` 的关联类型分支只认基类型是
  `Ty::Named` 的情况，泛型参数 `F` 还是推断变量时漏过，掉到 `resolve_type_path`
  当值路径查。现在：(1) 声明期 `F` 是推断变量 → 返回新推断变量占位，等实例化
  重算；(2) 实例化期 `F` 是具体类型 → `resolve_assoc_type` 走泛型 impl 路径
  （`generic_impl_assocs` 保留目标 AST 与泛型参数名，用 `match_generic_ast` 把
  `Ready<T>` 的 `T` 绑到实参，再把 `type Output = T` 的 AST 在新作用域里降低）。
  同时修了 `impl_target_matches` 用结构体 InferId 建替换表的 bug（impl 的 `T`
  与结构体的 `T` 是不同 InferId，泛型 trait 边界 `F: Future` 因此判不了
  `Ready<i32>` 满足），改用 `bind_infer_args` 直接从 impl 目标抓 Infer。还修了
  `build_instance_signature` 没推 `Self` 的 bug（泛型 impl 方法签名里的
  `Self` / `Self::Output` 在实例化重降低时找不到）。覆盖 `Self::Output`
  （泛型 impl 内）、`F::Output`（泛型参数）、非泛型 impl 的 `Type::Output`，
  回归 `tests/cases/118_assoc_projection.zr`。`std/alloc/async.zr` 的 `block_on<F:
  Future>` 签名现在能过类型检查；剩余阻塞是 `Box` 类型未实现（`*mut dyn Wake`
  的 vtable 分派已随 dyn MVP 落地，x86-64）。`<T as Tr>::Item` 限定写法已支持，见下一条。
- **限定路径 `<T as Trait>::Item`**：✅ 已实现（2026-11-02）——早先类型位置
  见到 `<` 就掉进基础类型分派，报 `expected type, found Lt`，所以只能在泛型参数
  上用双段命名路径 `F::Output`（它按基类型的 impl 表查 `Output`，**不校验 trait**，
  两份同名关联类型时也无法消歧）。现在 `AstType::Qualified`（`src/parser/ast.rs`）
  → `parse_type_prefix` 在 `Underscore` 之后、`impl`/`dyn` 之前识别 `<` 开头
  → `ast_to_hir_type` 降低 self 类型、降低 trait 类型（必须是 `Ty::Named`）→
  `direct_impls` 校验该 trait 确实被实现（不在就报
  `the trait `X` is not implemented for `Y``，负向 `29_qualified_trait_not_impl.zr`）
  → 复用 `resolve_assoc_type` 投影。声明期泛型参数未绑定（`Ty::Infer`）时返回
  新推断变量，等实例化重降签名再算（与 `F::Output` 同一策略）。
  **顺带修掉一个既有缺口**：词法器把 `<<` / `>>` 切成 `Shl` / `Shr`（为移位
  表达式），类型位置因此**嵌套泛型根本解析不了**——`Vec<Vec<i32>>` 报
  `expected '>' to close generic arguments: expected Gt, found Shr`，
  `Option<<T as Tr>::Item>` 连外层实参都起不来。新增
  `Parser::split_angle`（`src/parser/mod.rs`）在**类型语境**把 `Shl`→`Lt Lt`、
  `Shr`→`Gt Gt`，调用点只在 `parse_type_prefix`（前缀仅拆 `<<`）、
  `Ident`/`KwUpperSelf` 之后、`parse_generic_args`（首实参前与收尾处）——
  表达式解析里的 `Shl`/`Shr` 必须保持原样。回归
  `tests/cases/119_qualified_assoc.zr`（泛型函数返回限定投影、泛型 impl 方法
  签名里的 `<Self as Future>::Output`、具体类型限定注解）。**仍不支持**：
  限定写法**不按 trait 过滤 impl 表**（投影仍复用 `resolve_assoc_type` 的
  "按类型 DefId 查关联类型"路径），两个 trait 同时为同一类型定义同名关联类型
  时无法真正消歧。
- **`impl Trait` 参数位置（APIT）**：✅ 已实现（2026-11-02）——早先
  `ast_to_hir_type` 把 `ImplTrait` 一律降成 `opaque(impl_trait_N)`，参数位置的
  `impl Trait` 完全用不了：形参拿到不透明类型，实参核验报
  `expected opaque(impl_trait_1), found X`，体内任何字段 / 方法访问都报
  `cannot access ... on type opaque(impl_trait_N)`。现在
  `check_program`（`src/typeck/mod.rs:662-692`）在**声明收集之前**对
  `expanded.functions` 跑一遍 `desugar_apit`：参数类型里每个 `impl Trait`
  就地换成 `Named{path:["__apit_N"]}`，边界作为
  `GenericParam::Type("__apit_N", bounds)` 追加到 `f.generics`。之后签名检查、
  单态化、边界校验（`check_trait_bounds`）、MIR 全部按普通泛型函数走，
  不需要新开路径。递归进引用 / 指针 / 泛型实参 / 元组 / 数组 / 切片 /
  可选 / 错误联合，所以 `&impl Trait` 绑**内层**类型（`&T`，不是把引用
  本身当泛型参数）、`Vec<impl Trait>` 是 `Vec<T>`；`impl A + B` 多边界可用。
  `fn(..)` 函数指针类型里的 `impl Trait` 不处理（Rust 也不允许）。
  `check_trait_bounds` 的报错对匿名参数特判成 `required by a bound in `f``
  （早先会直接把内部名 `__apit_N` 塞进文案）。**返回位置（RPIT）不做改写**：
  那是真正的存在类型（`opaque`），改名换姓也变不成泛型参数。
  回归 `tests/cases/120_apit.zr`；负向 `tests/negative/30_apit_bound.zr`
  （`i64` 没实现该 trait，边界必须报错）。**仍不支持**：trait 声明里的方法
  签名不做这个改写（声明本身不参与单态化）。
- **`const` 泛型 `<const N: usize>`**：✅ 已实现（2026-11-02）——早先解析阶段
  `GenericParam::Const(name)` 丢掉声明类型、`parse_generic_args` 只走
  `parse_type()`（`Arr<3>` 报 `expected type, found IntLiteral(3)`），检查阶段
  `N` 只是"找不到的名字"（`[i32; N]` 报 `cannot find value N`）。现在：
  (1) **AST**：`Type::ConstInt(i128)` 借泛型实参的 `Vec<Type>` 通道带整数，
  `GenericParam::Const(String, Type)` 保留声明类型；
  (2) **HIR**：`Ty::ConstParam(String)` 是声明期占位（进 `type_params` 作用域），
  `Ty::ConstVal(i128)` 是具体实参（只在 `Ty::Named` 的 args 里出现），
  数组长度新增 `ArrayLen::Param(name)`——声明期不求值，实例化时由
  `subst_infer`（typeck）/ `subst_infer_ty`（MIR）按名字换成 `Const(具体值)`，
  `type_generic_params` 按「类型 + const」**全参数顺序**登记，位置对齐不变；
  (3) **推断**：`match_generic_ast` 的 Array 分支从实参数组长度反推、
  `bind_type_args` 同理（结构体字面量没写注解时 `N` 从字段数组字面量长度推）、
  显式 turbofish 走 `check_turbofish_call` + `check_const_arg_kinds`；
  推不出来报 ``cannot infer const parameter `N` for `f` ``；
  (4) **值位置**：`N` 在 `while i < N` / `N as i32` 里按 `const_param_tys`
  记录的声明类型降成整数字面量；`[0; N]` 的重复次数同理；
  (5) **种类核验**：`Arr<i32>`（类型塞给 const 参数）、`Vec<3>`（值塞给类型
  参数）、`N` 当类型用（`let x: N`）都在注解处报错。
  **仍不支持**：`[T; N + 1]` 这类含参数的常量表达式（长度表达式仍按编译期常量
  求值），以及类型侧的"省略实参自动推断"（结构体字面量只能从数组字面量长度推）。
  回归 `tests/cases/122_const_generic.zr`；负向 `32` / `33` / `34`。
- **`#[derive]`**：方法体已生成（`PartialEq`/`Eq`/`Hash`/`PartialOrd`/`Ord`/`Default`/`Debug`），
  `Hash` 已覆盖**指针/引用按地址**（`b5f5a56`）、**元组按内容**（`38f1ddd`）、
  **数组按内容**（`caab7a0`）、**`str`/切片按内容**（`41ec8f4`，`96_str_slice_hash.zr`）；
  `Debug` 也已生成 `.debug()` 方法体（`Type__debug(self) -> String`，Rust `{:?}` 风格，
  `100_debug_derive.zr` + 负向 `19_debug_float.zr`；浮点/指针/数组/元组/`String` 字段显式报错；
  **结构体变体载荷里的 `str` 字段与 `str` 后嵌套字段两处"组合错"经 `EnumField`
  聚合特判后也已对齐 Rust**，见 `AGENTS.md` §7 的 `Debug` 说明）；
  `Debug` 还生成了多行 `.debug_pretty()` / `.debug_pretty_at(self, __ind)`
  （Rust `{:#?}` 风格，`13acff2`；缩进是**运行期**参数——同一类型可能在不同
  嵌套深度，写死缩进会与 Rust 不一致，`108_debug_pretty.zr` 逐个对照
  `rustc {:#?}`），以及格式引擎入口 `Type__debug_fmt(*const void, bool, *mut u8, usize) -> usize`
  （把值按 `pretty` 渲染进调用方缓冲、返回所需长度；`io::fmt_debug` 拿它组装
  `FormatArg`，见 §4 的 `println` 条）；**剩**：泛型类型 / 带显式判别式的
  枚举 `Ord` 显式报错。
- **`#[derive]` 枚举的 `Default`**：✅ 已实现——早先直接报 "zr has no
  `#[default]` variant attribute"。现在 `Variant` AST 加了 `attrs` 字段、
  `parse_variant` 解析变体上的 `#[...]`，`derive_methods` 对枚举找恰好一个
  `#[default]` 变体（没有或多个都报错，与 Rust 一致），生成
  `Type__default()` 构造该变体，载荷字段按结构体同样的规则取零值 / 递归
  `__default()`（`default_enum_fn` + `payload_field_infos`）。
  回归 `tests/cases/116_enum_default.zr`；负向 `tests/negative/14_enum_default.zr`
  （无 `#[default]`）。
- **`union` 构造与字段访问**：✅ 已实现——早先 `union U { i: i32 }` 能声明但
  `U { i: 42 }` 报 `` `U` is not a struct ``、`u.i` 报 `no field i on type U`。
  三处接上：`check_struct_init` 对 `HirTypeDef::Union` 要求恰好一个字段（Rust
  语义）并按该字段类型检查实参；`struct_field_tys` 同时返回 union 字段，字段
  读 / 写走结构体同一套路径；MIR `struct_layout` / `type_size_align` 把 union
  所有字段放在偏移 0、尺寸 / 对齐取最大字段——类型双关（写 f32 读 bytes）天然
  成立。`ty_is_aggregate` 把 union 算作聚合（按地址传 / 返回）。覆盖构造、字段
  读写、类型双关、按值传参 / 返回，回归 `tests/cases/117_union.zr`。
- **`#[derive]` 枚举的 `PartialOrd` / `Ord`**：✅ 已实现——早先直接报
  "ordering needs discriminant comparison, which derive does not implement"。
  但**无显式判别式**的枚举，判别式就是声明下标，按声明顺序比即是正确语义，
  根本不需要求值任何表达式。现在生成两级 `match`（`lt_enum_fn`）：外层按
  `self` 的变体、内层按 `other` 的变体，不同变体按声明顺序定序，同一变体内
  对载荷字段做字典序比较（`payload_lt`，与结构体的 `lt_fn` 同一规则）；
  `le`/`gt`/`ge` 仍由 `lt`/`eq` 组合而来。**带显式判别式**（`A = 5`）的枚举
  继续显式报错（判别式顺序可能与声明顺序相反），负向钉子
  `tests/negative/26_enum_ord_discriminant.zr`；回归
  `tests/cases/114_enum_ord_derive.zr`（无载荷枚举全序、带载荷枚举的跨变体
  与同变体字典序、命名类型载荷递归进 `Key__lt`、方法与运算符一致性）。
- **`loop` 带值 break**：✅ 已实现（`87ef09a` + `daf9763`）——`let x = loop { break 7; }`
  不再静默得 0（`ExprKind::Break` 现在携带值，`Loop` 按其类型化）；
  `tests/cases/97_loop_value.zr`（含聚合 break 值）。`daf9763` 修掉聚合 break
  值在 `-O3`/`-Oz` 的寄存器分配缺陷（`live_intervals` 线性位置改按 RPO）。
- **`unsafe` 关键字**：✅ 已实现（`unsafe { ... }` 块可解析和通过类型检查；当前不改变语义，按块表达式透传）。
- **`yield` 协作让出点**：✅ 已从"类型占位"升级为可执行（2026-11-02）。
  语义是 **async fn 内的协作让出点**，不是生成器产出：`yield expr` 的操作数
  **照常求值**（副作用保留）、值丢弃、表达式类型 `()`；MIR 发一条 `Suspend`，
  由 `async_lower` 的既有状态机降成「保存活跃值 + `state=N` + `return Pending`」，
  `zr_run_tasks` 把 Pending 任务重挂队尾、下次轮询从 state 续体恢复。
  早先三个洞：(1) 表达式类型是 `Ty::Infer(InferId(0))` 占位（红线 2 禁止的
  "活推断变量当哨兵"）；(2) `let _ = inner;` 把检查完的操作数整体丢掉，
  `yield foo(p)` 里的 `foo` **从不被调用**——副作用静默消失；(3) 同步函数与闭包
  体里的 `yield` 静默当空操作（不报错也不做事）。现在 `ExprKind::Yield` 携带
  操作数（`Box<HirExpr>`），`substitute_local` / `remap_lifted_body` 都要下沉到
  它（否则闭包捕获跨函数内联读到外层帧）；`check_lambda` 显式把
  `current_is_async` 置假——闭包要么被提升成顶层函数、要么在调用点内联，
  两处都没有异步状态机，泄漏进来会让闭包体里的 `yield` 检查通过却永不生效。
  回归 `tests/cases/123_yield_cooperative.zr`（spawn 两 worker 交错顺序、
  操作数副作用、循环多轮让出 + 结构体/元组活跃值跨挂起点、tail 位置）；
  负向 `tests/negative/35_yield_sync.zr` / `36_yield_closure.zr`。
  **仍不支持**：生成器语义（`yield v` 产出值 + 调用方 resume 消费，需要新的
  生成器类型与 resume 协议）；`intrinsic!(yield)`（`std/alloc/async.zr::yield_now()`）
  在 typeck 就把 kind 丢成 `ExprKind::Intrinsic` 单位变体，MIR 无消费分支，
  且普通函数里的 `Suspend` 不会被状态机拆分——属独立工程。
- **trait 默认方法 / `default fn`**：✅ 已实现（2026-11-02）。trait 声明里
  **带方法体**的方法（包括 `default fn` 拼写）以前等于没写：`check_traits`
  只把方法登记成 `Trait::method` 签名，从不按 impl 目标发射，`lookup_method`
  按 `S__method` 查不到 → `s.method()` 报 ``no field `method` on type `S` ``。
  现在在 `check_program` 的 impl 展开阶段（`expanded.functions` 之前）为
  **非泛型 impl** 合成缺失的默认方法：克隆 trait 方法的 AST、把名字改成
  `Type__method`、登记 `impl_targets` / `impl_generics`，之后签名、函数体、
  MIR 全按普通方法走，不需要新分支。impl 写了同名方法就是覆盖（合成跳过）；
  默认方法体里调 `self.other()` 也走通——顺带修了方法接收者核验：
  `self` 在方法体里是 `&Self`，早先一律剥一层引用再比内层，`self.eq(other)`
  （`Eq::ne` 的默认体）报 `expected i32, found &i32`；现在接收者本身是引用时
  按**完整**形参类型核验（规则 3a 内层 unify 放行）。
  **同时补上缺失必需方法的诊断**：没有默认体的 trait 方法若 impl 没提供，
  在 impl 块上报 ``not all trait items implemented, missing: `x` ``（与 Rust
  一致），不再拖到调用点变成莫名其妙的 `no field`（负向
  `tests/negative/37_trait_missing_method.zr`）。
  回归 `tests/cases/124_trait_default_methods.zr`（常量默认体、默认体读字段 /
  调另一个默认体、impl 覆盖、trait 与 impl 两侧的 `default fn`、`&self`
  接收者核验）。**毯覆 impl 的方法解析已实现**（2026-11-06）：`lookup_method`
  查不到具名方法（`S__m`）时回退 `lookup_blanket_method`——扫描
  `impl_targets` 里 `impl_generics` 非空的方法，`ast_target_matches` 做
  只读结构匹配（`impl<T> Tr for T` 匹配任何类型、`impl<T> Tr for Vec<T>`
  只匹配 `Vec<..>`），匹配到的方法 DefId 交给 `check_generic_method_call`
  单态化。毯覆方法体里调 `T` 的方法（`self.cl()`，`T: Clone2`）、带边界
  毯覆（receiver 不满足边界时单态化报错，负向 `52`）、具体 impl 优先于
  毯覆（特化）、基元走毯覆都可用；回归 `tests/cases/172_blanket_impl.zr`。
  **仍不支持**：Rust 特化里"更具体的 impl 只覆盖一部分默认方法、其余从
  毯覆 impl 继承"的两级默认体继承——那需要默认方法合成与毯覆方法查找的
  组合（当前具体 impl 未覆盖的默认方法仍不会从毯覆继承）。
- **借用检查**：🟡 最小切片已实现——同位置借用重叠（`&mut`+`&mut` / `&`+`&mut`）
  报 `cannot borrow ... while already borrowed`；**写被借用位置**（`x = 5` 而
  `&x`/`&mut x` 存活）报 `cannot assign to ... while it is borrowed`；**读被借用
  位置**（`use x` 而 `&mut x` 存活）报 `cannot read ... while it is mutably
  borrowed`；**NLL 近似**（`let r = &mut x` 的借用绑定到 `r`、存活到 `r` 的
  最后使用，`finish_borrow_accesses` 在函数体检查完后统一判定）；
  `tests/negative/20_borrow_conflict.zr`、`21_borrow_write.zr`、`22_borrow_read.zr`、
  `tests/cases/105_borrow_nll.zr`。已知局限：多绑定 / 未关联借用按作用域结束、
  跨作用域重名可能混淆 last-use。
- **移动检查**：✅ 最小版已实现（`let b = a` 对含 drop glue 的类型是移动，读被移动的源报错）。
- **元数据**（权限、时间戳）与 `tempfile`：✅ 已补齐（`db0b01f`，ctime 于 2026-11-01 补）——
  `std/fs` 新增 `temp_file()`/`temp_file_raw()`（`/tmp/zr_tmp_<纳秒>`）、`file_mtime(path)`、
  `file_mode(path)`、`file_ctime(path)`（状态变更时间）；运行时新增 `zr_file_mtime`/
  `zr_file_mode`/`zr_file_ctime`（macOS x86-64 用 `stat$INODE64` 偏移 48/4/64，
  Linux x86-64 用 `newfstatat` 偏移 88/24/104，其它平台暂
  返回 -1）；`tests/cases/99_fs_tempfile_metadata.zr`（ctime 值已对照系统 `stat -f %c` 验证）。
  早先标注的 `fs::file_size(str)`
  "变量路径返回 -1"其实是**命名冲突**：`io::file_size(fd)`（`File::size` 的 fd 版）
  与它同名，撞成一个 `z_file_size` 符号、MIR 去重只留首个，调用点可能走到 fd 版
  （str 被当 fd 读）。已修：`io::file_size` 改名 `file_size_fd`，`fs::file_size`
  现在是唯一的路径版（`99_fs_tempfile_metadata.zr` 加字面量/变量路径断言）。
  同类冲突一并修掉：`slice::max/min`（与 `core::max/min` 二元版同名，`use
  core::max` 会绑到先收集的 slice 版、`max(a,b)` 静默拿 `Option`）改名
  `max_slice/min_slice`；`net::read_line`（与 `io::read_line` stdin 版同名）改名
  `read_line_stream`（net.zr 目前有 `#[repr(C, align(4))]` 解析问题、加载不了，
  改名为将来消除隐患）。根因是扁平符号表 + 路径导入不区分模块——`use core::max`
  取 `symbols["max"]`（先收集者），不是 core 模块里的那个；根治需按模块限定。
- **格式串**：命名参数 `{name}` 已实现（`println_fmt_named`，见下）；多行聚合
  Debug 已由 `#[derive(Debug)]` 生成 `.debug_pretty()`（`13acff2`）并接入格式
  引擎（`debug_fmt` 通道，`112_fmt_debug.zr`，见 §4 的 `println` 条）。
  **2026-11-01 定根因（此前"代码生成器寄存器/局部槽别名缺陷"的结论全部作废）**：
  该 bug 与代码生成无关，真凶在 `std/alloc/string.zr::push_bytes` 自己的 `reserve`
  算错。`push_bytes` 追加 `bytes` 后要保证 `capacity >= len + bytes.len`，却把
  增量传成了 `reserve(new_len - capacity)`（`new_len = len + bytes.len`）；
  而 `reserve(additional)` 只保证 `capacity >= len + additional`，于是只有当
  `len >= capacity`（通常 `len == capacity`）时才够、其余情况一律**少 reserve**，
  紧接着的拷贝循环 `self.ptr[self.len + i] = bytes.ptr[i]`（`i < bytes.len`）
  越界写堆。症状分两层：字面量对象恰好紧邻一块刚 `realloc` 过的 String 缓冲
  时，越界写把栈上 `str` 局部（`lit`）的 `ptr` 覆写成 `s.ptr`——`push_bytes`
  于是从 String 缓冲拷 `ABCD`+NUL（早先探针看到 `[r13] = s.ptr` 正是这个，
  并非寄存器定错）；最坏情况直接 `SIGABRT`（`/tmp/hand17.zr` 退回未修版实测
  exit 134，修复后五档全输出 `Outer {ABCDEF}`）。
  **修复**：改传 `reserve(bytes.len)`（`self.capacity` 比较仍用 `new_len > capacity`）。
  **回归**：`tests/cases/107_string_push_bytes_reserve.zr`（循环里 realloc +
  之后 `push_bytes` 多字节字面量）。**代码生成器无缺陷**——`{:#?}` 多行 Debug
  不再被"编译器 bug"阻塞：derive 侧（`13acff2`，`108_debug_pretty.zr`）与格式
  引擎侧（`debug_fmt` 通道，`112_fmt_debug.zr`）均已落地。
- **格式串命名参数 `{name}`**：✅ 已实现——`println_fmt` 是函数不是宏，`{name}`
  需要按名查参。新增 `io::println_fmt_named(fmt, &[NamedArg], n)`（`NamedArg { name: str, value: FormatArg }`），
  名字与值放同一个结构体（zr 里 `&["x","y"]` 无法强转成 `*const str`，分开两个
  数组不行）。`{name}` 在 `args[0..nargs]` 里按名字查表；名字没匹配按字面
  `{name}` 输出（zr 的格式化是运行期函数，静默保留原文比输出错值安全）；
  支持规格（`{x:>5}`、`{x:x}`）与重复使用同一个名字（`{x}..{x}`）。
  `io_print_matches_rust` 已加 `{x}+{y}={x}` / `nmiss={unknown} {x}` / `spec=[{x:>5}] 0x{x:x}`
  三组钉住。
- **闭包**：**无捕获**闭包已可存进变量/当实参/返回（提升成 `__closure_N` 顶层函数指针）；**带捕获**闭包已可存进变量/嵌套/多次调用（`tests/cases/84_closure_capture_value.zr`）；**跨函数传 env 已支持**——把环境结构体按值当实参传给**泛型**形参（`fn apply<F>(f: F, ..)`，`F` 单态化到 `__env_N`，被调函数体的 `f(x)` 按环境 DefId 内联展开，`tests/cases/109_closure_env_cross_fn.zr`）。跨函数内联靠 `substitute_local` 把捕获 local 替换成形参 `f.__cN`——**该函数必须递归进 `Index` 的基址、`Array`/`Tuple` 元素**，早先 `Index(_, v)` 只替换下标，数组捕获跨函数内联读到别的栈帧直接 SIGSEGV（同函数内联因外层 local 还在而"碰巧"对）。
  **P1-2c：带捕获闭包转裸 `fn(..)` 值** ✅ 已实现（2026-11-02）。早先传给 `fn(..)` 形参报 `captured closure cannot be passed as a plain `fn` value`（负向 `07`）、作 `fn(..)` 返回值报 `captured closure cannot be returned as a plain `fn` value`（负向 `25`；早先编译通过、运行 SIGSEGV），两个负向用例已随实现删除。现在**函数值 = 16 字节聚合记录 `{code, env}`**：
  (1) `Ty::Function` 从标量改成聚合（MIR 里聚合以地址表示），`type_size_align = (16, 8)`、`struct_layout` 返回 `code`@0 / `env`@8、`is_scalar_ty` 移除 Function、`element_size = 16`、`infer.rs::size_of = 16`；`std/std/thread.zr::size_of_thread_handle` 随之 16 → **24**。
  (2) **统一 shim（thunk）ABI**：`code` 恒指向「末尾多收一个 `env`」的 shim，间接调用统一按 `(用户实参..., env, [隐藏返回槽])` 传参。**env 必须在 retptr 之前**——MIR 给聚合返回函数追加隐藏返回槽的位次正是 `params.len()`，而 shim 声明的形参就是 `(用户形参..., env)`，两边自动对齐。
  (3) 新 HIR 节点（`src/typeck/hir.rs`）：`ExprKind::FnValue { func, shim }`（函数名作一等值）、`ClosureToFn { env, thunk }`（带捕获闭包转函数值）、`FnToCodePtr(Box<HirExpr>)`（记录退化裸代码地址）。`check_expr` 末尾 `coerce_closure_to_fn` 在期望类型 resolve 成 `Ty::Function` 且实参是 `__env_N` 时生成 `ClosureToFn`；`Expression::Return` 与 `check_call` 两处原有的「不能当函数值」报错删除。
  (4) `finalize_fn_value_shims`（`src/typeck/mod.rs`，`check_function_bodies` 收尾在 `finalize_lifted_closures` 之后调用）：`std::mem::take(&mut self.functions)` → 所有函数体跑 `rewrite_fn_values_block`（把 `ExprKind::Def(id)` 就地改成 `FnValue`）→ 逐个 `pending_shims` 落成 shim。普通函数 shim = `shim(args..., env) { return func(args..); }`（env 忽略）；带捕获闭包 thunk 把捕获 local 换成 `env.__cN`；**环境在 MIR 降级时用 `zr_alloc` 复制到堆**（调用方可能立刻返回，栈上环境活不到被调方使用它的时刻）。
  (5) `FnToCodePtr` 只用于 `extern "C"` 边界：内层是 `FnValue` 时直接取 `FuncAddr(func)`（**绕开 shim**，否则 C 侧传的实参会被当成 env、整体错位），否则读记录 +0；节点类型刻意标 `Ty::Pointer(Unit, Immutable)`（`*const void`），避免 MIR 按 16 字节聚合再复制一遍。
  (6) 明确报错：泛型函数当值报 `cannot use a generic function as a value: generic functions are monomorphized at named call sites only`（负向 `38_generic_fn_as_value.zr`）、async 函数当值报 `cannot use an async function as a value: async functions return a future frame and have no plain call address`（负向 `39_async_fn_as_value.zr`）。普通函数值**不做 `zr_alloc`**（`FormatArg.debug_fn` 每次格式化都会构造，会泄漏）。
  回归 `tests/cases/125_closure_to_fn.zr`（闭包直传 `fn(..)` 形参 / 作返回值 / `let f: fn(..) = |..|` 跨函数调用 / 普通函数当值 / 返回的闭包再传出 / 循环里反复取带捕获闭包 / 函数值存结构体字段 / 捕获数组与元组）。**仍不支持**：带捕获闭包经 `extern "C"` 传给 C 侧回调（C 侧没有地方放 env）。
- **`i128`/`u128`**：✅ 已在 64 位目标（x86-64）上打通——拆成两个连续 8 字节栈槽
  （lo / hi），加减 `adc`/`sbb`、位运算逐字、移位 `shld`/`shrd`、比较按高位优先
  定序，传参 / 返回走 `rdx:rax`，字面量 / 拷贝 / 转换可用；**乘 / 除 / 取模也已
  实现**——乘拆四段 64 位部分积（两个跨字项的高半累加进 `hi`），除 / 取模走
  128 轮软件长除（商每轮左移、余数 `(rem<<1)|carry` 左移进被除数位、`sub/sbb`
  比较、借位恢复；有符号按"两符号异或的最低位"定商符号、余数符号跟随被除数）。
  `regalloc::assign_int_widths` 按「类型宽于机器字」标宽值，宽值不占寄存器池、
  统一走连续栈槽。**32 位目标仍禁用**（类型检查阶段报错）。
  回归 `tests/cases/111_i128.zr`（加减/位/移位/比较/转换）与
  `tests/cases/113_i128_muldiv.zr`（乘除取模，含**跨函数**的宽实参）。
- **泛型推断**：✅ 已改进（`bab298c`）——`try_instantiate_call` 推不出类型参数时
  留推断变量、等用法体约束（`let v = Vec::new(); v.push(1)` 不再报
  `cannot infer type parameter`）；`check_instance_body` 里若仍停在未解析推断变量
  才报错（`tests/cases/98_generic_infer_usage.zr`，负向
  `tests/negative/18_generic_infer_unused.zr`）。
- **`Vec<T>` 下标 `v[i]`**：✅ 已实现——早先 `check_index_scalar` 只认数组/切片/
  `str`/指针，`v[i]` 报 `cannot index a value of type Vec<u8>`（`net.zr` 的
  `buf[i]` 因此加载不了）。现在 `v[i]` 脱糖成 `v.data[i]`（`data` 是 `*mut T`
  字段，指针下标读/写都走指针算术，元素 = `data` 指向的类型）；
  `tests/cases/101_vec_indexing.zr`（读/写、`i64` 元素宽度）。
- **路径调用的 `?` 与泛型路径调用**：✅ 已修两个类型检查 bug——
  (1) `infer_expr_ty` 早先没有 `Expression::Path` 分支，`TcpListener::bind(addr)?`
  这类**多段路径**调用在 `?` 处看不到返回类型，退化成"当前函数返回类型"兜底，
  `listener` 被推断成 `()`、`.accept` 报「tuple index out of range」；
  现在 `Path` 分支把路径解析成函数类型、`?` 拿到真实载荷。
  (2) `generic_call_target` 早先只认 `Variable`，`Vec::new()` 这类**泛型路径**
  调用的快速推断拿到**未替换**的泛型签名（`Vec<T>` 里 `T` 是共享占位符），
  同文件另一个 `Vec<SocketAddr>` 实例化把占位符绑成 `SocketAddr`、
  `req.push(u8)` 报「T inferred as both SocketAddr and u8」；
  现在 `generic_call_target` 也认 `Path`、泛型路径调用返回新推断变量。
  `tests/cases/102_path_question.zr`（两者都钉住）。
- **内联模块的限定路径访问**：✅ 已实现——`mod af { pub const INET }` 的条目被
  扁平并入顶层、`af` 本身不是符号，早先 `af::INET` 报「cannot find value `af`
  in this scope」（`net.zr` 的 `family: af::INET` 因此加载不了）。现在内联模块名
  单独登记（`flattened`，与**加载**模块 `known_modules` 区分），`resolve_path`
  看到首段是内联模块时把叶子段拿到顶层符号表解析：`af::INET` → 顶层 `INET`。
  `tests/cases/103_inline_mod.zr`（常量 + 函数都钉住）。
- **`net` 模块可加载 + BSD 套接字运行时**：✅ 已实现——`std/std/net.zr` 是一个从未
  被测试加载的破损标准库模块，早先它在类型检查阶段就挂掉（`#[repr(C,
  align(4))]` 语法、缺 `Vec` 导入、`unwrap_or_else` 实参是值非闭包、内联模块
  `af::INET` 无法解析、`into_socket` 从 Drop 类型移字段）。修完后 `net` 能通过
  类型检查并生成目标文件，但链接仍失败——`net.zr` 声明的 `zr_socket` /
  `zr_bind` / `zr_accept` 等 BSD 套接字封装在运行时里没有对应实现
  （`undefined symbol: _zr_accept`）。已给 macOS x86-64 运行时补齐 19 个套接字
  封装（`zr_socket` / `zr_bind` / `zr_connect` / `zr_listen` / `zr_accept` /
  `zr_recv` / `zr_send` / `zr_setsockopt` / `zr_getsockopt` / `zr_getaddrinfo` /
  `zr_freeaddrinfo` / `zr_inet_ntop` / `zr_inet_pton` / `zr_htons` / `zr_ntohs` /
  `zr_htonl` / `zr_ntohl` / `zr_set_nonblocking` / `zr_errno`，均薄封装 libSystem
  的 BSD 套接字 API），`net` 现在全链路可加载并运行（创建 + 关闭套接字实测）。
  **macOS arm64 版已同样补齐**（`b _socket` 尾调用；`zr_set_nonblocking` 用
  callee-saved x19/x20 跨两次 `fcntl`；`zr_errno` 经 `___error`；19 个符号经
  `clang -target arm64-apple-macos` 汇编验证、重定位指向正确 libc 符号，本机
  是 x86-64 无法运行验证）。
   **Windows（Winsock）版已同样补齐**：`rt_windows_x86_64.s` 与
   `rt_windows_arm64.s` 的 19 个封装走 `ws2_32.dll`（`socket`/`bind`/`connect`/
   `listen`/`accept`/`recv`/`send`/`setsockopt`/`getsockopt`/`getaddrinfo`/
   `freeaddrinfo` 同名导出；`inet_ntop`/`inet_pton` 在 Winsock2 里叫
   `InetNtopA`/`InetPtonA`；`htons`/`ntohs`/`htonl`/`ntohl` 不是 ws2_32 导出
   （头文件内联字节交换），运行时自己实现（`rol`/`bswap`）；`set_nonblocking`
   用 `ioctlsocket(FIONBIO)`（BSD 的 `fcntl` 在 Windows 不存在）；`zr_errno`
   用 `WSAGetLastError`）。每个走 Winsock 的封装先经 `zr_wsa_init` 惰性
   `WSAStartup`（`.bss` 里的 flag 保证只调一次，直接调 bind/connect 也不会拿
   `WSANOTINITIALISED`）。MS x64 ABI（rcx/rdx/r8/r9 + 32 字节 shadow）与
   Windows ARM64 ABI（x0..x3 + 第 5 参在 `[sp]`）分别处理；PE 链接命令加
   `ws2_32`（clang `-lws2_32`、lld-link `ws2_32.lib`）；19 个符号经
   `clang --target=x86_64/aarch64-pc-windows-msvc` 汇编验证、16 个 Winsock
   extern 引用正确，本机无 Windows 无法运行验证。**裸机（ARM/RISC-V）没有
   操作系统与网络栈，套接字无法实现**——运行时未引用任何套接字符号，裸机
   程序若引用 `zr_socket` 会正确得到链接错误（未定义符号），这已是预期失败
   模式，不提供桩函数。  **Linux 运行时是纯 syscall 设计（不依赖 libc），
  `getaddrinfo` / `inet_ntop` / `inet_pton` 不是 syscall、无法原样实现**——要
  么给 Linux 运行时接 libc，要么这 3 个在 Linux 上返回错误；未做。
  `tests/cases/104_socket_runtime.zr`（钉住）。
- **穷尽性报错**：✅ 已改进（`a526953`：枚举缺臂列出缺失变体名，`17_match_missing_variant.zr`；整型/标量仍保留尾 `_` 臂措辞）。
- **调用实参 / 字段值的类型核验**：✅ 已实现（2026-11-02）——早先实参核验是个静默洞：
  `check_method_call` 缺参时拿 `InferId(0)` 哨兵兜底、实参整体没有核验路径，
  `std/std/error.zr` 里字面量直传 `String__push_str(&String)` 这类"16 字节值的高 8
  字节被当指针用"的必然 SIGSEGV 代码静默通过。现在统一走
  `check_call_arg` / `check_field_value`：先精确 `unify`，引用/指针形状再按
  **内层** unify 绑推断变量（`push_slice(&mut req, ..)` 的 `Vec<_>` 才有机会
  绑到 `u8`），最后落 `ty_equiv_relaxed` 宽松等价——整数族宽度差异、切片家族
  `{ptr,len}`（`str` ↔ `[]const u8`）、引用↔裸指针、聚合值↔引用（方法
  receiver）、**数组退化**（`&[FormatArg; 2]` → `*const FormatArg`，只单向）。
  **方法 receiver 有自动取引用语法糖**：裸 `self` 由 `receiver_ty` 降成
  `&Self`（zr 约定，不同于 Rust 的按值 self），标量 receiver 也走这条
  （`a.lt(b)` 的 `a: i64` 对 `self: &i64`），核验前剥一层引用；**普通调用
  没有这个语法糖**（`g(v)` 传 `fn(&i64)` 仍报错，负向 `27`；`str` 塞
  `&String` 报错，负向 `28`）。顺带修了三个被核验暴露的类型检查 bug：
  (1) `?` 的载荷标注早先取自启发式兜底（`infer_expr_ty` 没有 Field 分支时
  兜成当前函数返回类型），改用 `check_expr` 之后 `expr.ty` 的真实类型实参；
  (2) `lower_pattern` 的 Tuple 分支不 resolve 就拆，`let (a, b) = f()?;`
  的子绑定拿到整个元组；(3) `ty_equiv_relaxed` 规则 3a 早先直接 `return`
  递归结果，数组退化（3c）永远轮不到。核验暴露的既有错误点已逐个修正
  （`std/std/error.zr` 调用点改 `push_bytes`、`std/alloc/mem.zr` 的
  `Layout::from_size_align` 返回 `Result`）。`std/error` 因此可加载
  （std 模块 12→13，见 §2）；回归 `cargo test` 三套件全绿。
- **文档债**：✅ 已清（`fb4dfb2` 修 `tests/README.md` / `future/README.md`；`1d38ca3` 修 README 安全模型节 / `rust-parity.md` / `CONTRIBUTING.md` / `Cargo.toml` description）。

## 6. 工作约定

- **提交信息用中文**，Conventional Commits（`feat:` / `fix:`）。
  本项目提交信息风格是**详细中文段落**：先讲"早先怎么错"，再讲"为什么这么改"，
  最后 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。照这个写。
- 注释用中文，密度与现有代码一致（本项目注释很密，改代码时顺手补上下文是对的）。
- 加语言特性按顺序：`lexer`（token）→ `parser` → `typeck` → `mir` → `ssa` →
  `codegen` → `std/` → `tests/cases/`。**跳步会让下游拿到占位数据。**
- 改完 `cargo build` 通过 **≠** 改对了。语言改动至少 `cargo test --test zr_language` 一遍；
  类型检查改动要额外确认负向用例（该报错的还是报错，用 `/tmp/x.zr` 手工验）。
