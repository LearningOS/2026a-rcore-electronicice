# Lab 1：多道程序与分时多任务

## 实现总结

本实验实现了 `sys_trace` 系统调用：支持读取当前任务指定地址的一个字节、向指定地址写入一个字节，以及查询当前任务对指定系统调用的累计调用次数。每个任务在 TCB 中维护独立的计数表，并在系统调用统一分发入口先计数、后执行，因此查询 `sys_trace` 时也会包含本次调用。

## 问题的来源与共同心智模型

这些问题来自 `rCore-Tutorial-Guide/source/chapter3/5exercise.rst`。第一组要求运行 `rCore-Tutorial-Test` 提供的三个 `ch2b_bad_*` 程序，用故意出错的用户程序验证权限边界。第二组七个问题是按照 `trap.S` 的执行顺序拆分的：问题 1—5 研究内核如何回到用户程序，问题 6—7 研究用户程序如何进入内核。题目中的行号对应教程当时的文件；当前仓库行号可能不同，应以指令内容为准。

所有小实验共用一个闭环：

1. **触发**：用户程序执行一条系统调用、特权指令或错误内存访问。
2. **CPU 接管**：硬件记录 Trap 原因和返回地址，将特权级切换为 S 态，跳到 `stvec`指定的 `__alltraps`。
3. **软件保存现场**：`__alltraps` 切换到内核栈，构造 `TrapContext`，调用 `trap_handler`。
4. **内核决策**：`trap_handler` 根据 `scause` 处理系统调用、杀死错误任务或调度其他任务。
5. **恢复或继续**：`__restore` 恢复寄存并用 `sret` 返回 U 态；若当前任务被杀死，则调度下一个任务。

## Bad 程序：三个权限边界小实验

### 实验 0：先建立可重复的观察环境

**目的**：只编译第 2、3 章的基础测例，让三个 Bad 程序与普通程序一起运行。这样不仅能看到错误，还能验证错误后系统是否继续运行。

```bash
docker exec rcore-container-qemu8 bash -lc \
  'cd /mnt/os && make run CHAPTER=3 TEST=3 BASE=1'
```

**实际环境**：QEMU 启动时输出 `RustSBI version 0.4.1`。运行结果中先后出现一次 PageFault 和两次 IllegalInstruction，之后 `hello_world`、幂运算和三个 yield 程序仍继续并完成。

### 实验 A：`ch2b_bad_address` 为什么会出现 PageFault？

**程序怎么来的**：测试作者需要一个最小的“用户程序访问不可用内存”实例，因此它把 `0x0` 转成 `*mut u8`，再通过 `write_volatile(0)` 强制产生一次真实的字节写入。使用 volatile 是为了防止编译器将这次写入优化掉。

**预测**：零地址不是该用户程序的有效可写位置，CPU 应产生 StoreFault 或 StorePageFault；`panic!("FAIL")` 不应被执行。

**代码路径**：

```text
(0x0 as *mut u8).write_volatile(0)
  → CPU 产生存储异常
  → __alltraps
  → trap_handler 的 StoreFault | StorePageFault 分支
  → exit_current_and_run_next()
```

**实际观察**：

```text
[kernel] PageFault in application, bad addr = 0x0,
bad instruction = 0x804003a4, kernel killed it.
```

**结论**：测试的核心不只是“零地址会错”，而是“用户程序出错后控制权回到内核，内核只结束当前任务”。后续普通程序继续输出，证明内核没有一起崩溃。

### 实验 B：`ch2b_bad_instructions` 为什么要在 U 态执行 `sret`？

**程序怎么来的**：`sret` 是内核用来从 S 态返回的特权指令。测试作者故意在普通用户程序中内联汇编 `sret`，直接验证 U 态是否真的受限。

**预测**：U 态无权执行 `sret`，CPU 应产生 IllegalInstruction；指令之后的 `panic!("FAIL")` 不应运行。

**代码路径与观察**：

```text
U 态执行 sret
  → IllegalInstruction
  → __alltraps
  → trap_handler 的 IllegalInstruction 分支
  → exit_current_and_run_next()

[kernel] IllegalInstruction in application, kernel killed it.
```

**结论**：同一条 `sret` 在内核 S 态中是正常的返回指令，在用户 U 态中却是非法指令。“是否合法”由当前特权级与指令要求共同决定。

### 实验 C：`ch2b_bad_register` 为什么读 `sstatus` 也会出错？

**程序怎么来的**：前一个实验测“特权指令”，这一个测“特权寄存器”。它用 `csrr {}, sstatus` 尝试在 U 态读 S 态控制状态寄存器。

**预测**：U 态无权读 `sstatus`，CPU 应产生 IllegalInstruction，因此打印 `sstatus` 的 `panic!` 不应执行。

**代码路径与观察**：

```text
U 态执行 csrr ..., sstatus
  → IllegalInstruction
  → __alltraps
  → trap_handler 的 IllegalInstruction 分支
  → exit_current_and_run_next()

[kernel] IllegalInstruction in application, kernel killed it.
```

**结论**：用户程序既不能执行 S 态特权指令，也不能读取 S 态 CSR。这保证用户程序不能自己修改返回特权级、Trap 入口等内核控制状态。

## `__alltraps` 与 `__restore`：七个闭环小实验

### 实验 1：刚进入 `__restore` 时，`sp` 到底指向什么？

**问题为什么会出现**：`__restore` 同时被“Trap 处理完毕”和“任务第一次运行”使用，所以要确认两条路径是否都提供同样格式的 `sp`。

**实验方法**：反向查找 `__restore`、`goto_restore` 和 `init_app_cx`：

```bash
rg -n "__restore|goto_restore|init_app_cx|push_context" os/src
```

**观察 1：Trap 返回路径**。`__alltraps` 先执行 `addi sp, sp, -34*8`为 `TrapContext` 分配空间，保存数据后调用 `trap_handler`。`call trap_handler` 返回时会顺序落到紧接着的 `__restore`，此时 `sp` 仍指向刚才构造的 `TrapContext`。

**观察 2：首次运行路径**。`init_app_cx` 调用 `KernelStack::push_context`，在内核栈顶预先放入一份 `TrapContext`；`TaskContext::goto_restore` 再把 `ra` 设为 `__restore`、把 `sp` 设为这份 `TrapContext` 的地址。

**结论**：两条路径在进入 `__restore` 时都满足同一个接口契约：`sp = &TrapContext`。所以同一段恢复代码既能“恢复老现场”，也能“伪造一份新现场”启动任务。

### 实验 2：`ld` 和 `csrw` 特殊恢复了什么？

**问题为什么会出现**：通用寄存器可以批量恢复，但“返回到哪里”和“返回到哪个特权级”需要 CSR 配合。

**实验方法**：对照 `TrapContext` 的 `#[repr(C)]` 字段顺序和汇编偏移。在 64 位 RISC-V 中每项 8 字节：

| 汇编 | `TrapContext` 位置 | 恢复到 | 意义 |
| --- | --- | --- | --- |
| `ld t0, 32*8(sp)` | `x[32]` 之后的 `sstatus` | `csrw sstatus, t0` | `SPP` 决定 `sret` 后的特权级 |
| `ld t1, 33*8(sp)` | `sepc` | `csrw sepc, t1` | `sret` 后 PC 从哪条用户指令继续 |
| `ld t2, 2*8(sp)` | `x[2]`，即用户 `sp` | `csrw sscratch, t2` | 暂存用户栈，等最后一步交换 |

**闭环观察**：`TrapContext::app_init_context` 将 `sstatus.SPP` 设为 User、`sepc` 设为应用入口、`x[2]` 设为用户栈顶；应用最终确实从入口在 U 态运行。

**结论**：这三项不是普通业务数据，而是“返回环境”：`sstatus` 决定身份，`sepc` 决定位置，`x[2]` 决定返回后使用哪个栈。

### 实验 3：为什么批量恢复要跳过 `x2` 和 `x4`？

**假设**：如果立即用 `ld x2, 2*8(sp)` 恢复 `x2`，`sp` 会在恢复过程中突然从内核栈变成用户栈，后续的 `ld ...(...sp)` 就会从错误的栈读数据。

**实验方法**：按顺序模拟 `__restore`。在所有 `ld` 完成前，每一条都使用 `sp` 加偏移读取 `TrapContext`，因此 `sp` 必须始终保持为内核栈指针。用户 `sp` 先被放到 `sscratch`，直到最后才交换。

`x4` 是 `tp`。当前章节明确假设应用不使用它，所以没有保存和恢复。这是章节简化，不是所有操作系统都可以忽略 `tp`。

**结论**：跳过 `x2` 是恢复算法的必要条件；跳过 `x4` 是本章应用 ABI 的简化假设。两者“都跳过”，但原因不同。

### 实验 4：`__restore` 末尾交换 `sp` 与 `sscratch` 后发生了什么？

**实验方法**：把 `csrrw sp, sscratch, sp` 当成一次原子交换，手工填表：

| 时刻 | `sp` | `sscratch` |
| --- | --- | --- |
| 执行前 | 释放 `TrapContext` 后的内核栈顶 | 从 `TrapContext.x[2]` 取出的用户栈指针 |
| 执行后 | 用户栈指针 | 当前任务的内核栈顶 |

**闭环**：交换后紧接着执行 `sret`，用户程序能正常调用 Rust 函数和访问局部变量，证明 `sp` 已经是用户栈。下次 Trap 又能通过相同指令取回内核栈，证明 `sscratch` 保留了内核栈顶。

**结论**：这条指令是“归还用户栈，同时为下一次 Trap 保存内核栈入口”。

### 实验 5：哪条指令真正完成 S 态到 U 态的切换？

**假设**：恢复寄存器和交换栈都只是准备，真正改变 CPU 特权级的是 `sret`。

**证据链**：

```text
TrapContext::app_init_context 将 sstatus.SPP 设为 User
  → __restore 将它写回 sstatus
  → __restore 将应用入口写入 sepc
  → sret 读取 SPP，把特权级恢复为 U
  → PC 跳到 sepc，用户程序开始执行
```

**反向实验**：`ch2b_bad_instructions` 证明 U 态执行 `sret` 会变成 IllegalInstruction；而内核 `__restore` 执行同一指令后，屏幕出现用户程序输出。

**结论**：`sret` 是状态切换点。之所以进入 U 态，不是因为指令名字固定表示 U 态，而是因为之前恢复的 `sstatus.SPP` 值是 User。

### 实验 6：`__alltraps` 开头交换 `sp` 与 `sscratch` 后发生了什么？

**问题为什么会出现**：CPU 发生 Trap 时不会替内核自动切换 Rust 栈。刚进入 `__alltraps` 时，`sp` 仍然是用户栈，内核不应在不可信的用户栈上保存自己的上下文。

**实验方法**：用实验 4 的表格反向模拟：

| 时刻 | `sp` | `sscratch` |
| --- | --- | --- |
| Trap 刚发生 | 用户栈指针 | 内核之前留下的内核栈顶 |
| 执行 `csrrw` 后 | 内核栈顶 | 原用户栈指针 |

之后 `addi sp, sp, -34*8` 才能在内核栈上安全分配 `TrapContext`；原用户 `sp` 从 `sscratch` 读出并保存到 `TrapContext.x[2]`。

**闭环观察**：三个 Bad 程序都是在用户栈上运行时突然触发异常，但内核仍能正常执行 `trap_handler`、打印错误并调度下一个任务，说明 Trap 入口已成功换到内核栈。

**结论**：这是 U 态进入内核后软件执行的第一次栈切换，它与实验 4 的指令互为逆过程。

### 实验 7：从 U 态进入 S 态究竟是哪一条指令？

**问题为什么会出现**：Rust 函数 `trace_read → trace → sys_trace → syscall` 仍是用户态内的普通函数调用。函数名里有 `sys` 不代表 CPU 已经进入内核；特权级改变发生在汇编 `ecall`。

**实验输入**：`ch3_trace` 中局部变量 `var = 111u8`，执行 `trace_read(&var)`。

**完整数据与控制链**：

```text
trace_read(&var)
  → trace(Read, &var, 0)
  → sys_trace(0, &var, 0)
  → syscall(410, [0, &var, 0])
  → a7=410, a0=0, a1=&var, a2=0
  → ecall
  → CPU 产生 UserEnvCall，U 态 → S 态，PC 跳到 __alltraps
  → __alltraps 保存寄存并调用 trap_handler
  → trap_handler 将 sepc += 4，避免返回后重复 ecall
  → syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12]])
  → 内核 sys_trace(0, &var, 0) 读出 111
  → 返回值写入 TrapContext.x[10]，即保存的 a0
  → __restore 恢复现场
  → sret，S 态 → U 态
  → 用户态 syscall 函数从 ecall 之后继续，a0 作为返回值 111
  → trace_read 返回 Some(111)
```

**运行闭环**：`ch3_trace` 中的 `assert_eq!(Some(111), trace_read(...))` 通过，最终输出 `Test trace OK!`。

**结论**：`ecall` 是该系统调用中 U 态进入 S 态的硬件触发点；`__alltraps` 是切换后的软件入口。两者不是同一件事：前者改变 CPU 状态，后者保存程序现场并将控制流接到 Rust 内核。

## 实验测试记录

- 本地 `ch3_trace` 单项测试通过，输出 `Test trace OK!`。
- 本地将时间测试、`ch3_trace` 和三个 yield 程序组合运行时全部通过。
- GitHub Actions 第一次评测为 `1/7`：最初把每个任务的系统调用计数表设置为 512 项，在官方多程序环境中导致 `ch3_sleep` 首次取得的时间为 0，随后 QEMU 超时。
- 按实验指导将 `MAX_SYSCALL_NUM` 修正为 500 后，本地使用课程固定版本的官方 Checker 和 Test 重新评测，七项检查全部通过，输出 `Test passed: 7/7`，报告检查同时通过。

## 实验体会

核心内容在于 对 rust 工程的系统抽象理解，以及对本项目已有框架的抽象理解。
具体的代码在现阶段反而较为容易，只要准确理解意图，并且自动化测试即可。

## 荣誉准则

1. 在完成本次实验的过程（含此前学习的过程）中，我曾分别与 **GPT5.6-sol deepseek-V4.1flash** 就（与本次实验相关的）以下方面做过交流，还在代码中对应的位置以注释形式记录了具体的交流对象及内容：

   - GPT5.6-sol：讨论了 `sys_trace` 的调用链、按任务保存系统调用计数的设计，完成了相关知识的原始积累。Rust 编译错误定位、Docker/QEMU 测试方法与实验报告结构。讨论了系统抽象和rust工程语法，讨论并且完成了，在syscall入口的计数设计，以及增加和读取id统计表格的抽象设计。完成了语法错误的修改，以及手误小错误的小修改，目前没有逻辑错误的修改。
   - deepseek-V4.1flash：帮我完成了自动编译和启动docker的脚本。


2. 此外，我也参考了 **以下资料**，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

   - LearningOS《rCore-Tutorial-Guide》第三章实验指导与荣誉准则。
   - 本仓库 `README.md`、`os/src/trap/trap.S`、`os/src/task/` 和 `os/src/syscall/` 源码。
   - `rCore-Tutorial-Test` 中的 ch3 用户测试，以及 GitHub Actions 官方检查器输出。

3. 我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。
   我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响其评分。

4. 我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。
   我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。
   我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。
   我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。
