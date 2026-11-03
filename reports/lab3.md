# Lab 3：进程管理与 Stride 调度

## 实现总结（200 字以内）

本实验实现了 `spawn`、`set_priority` 和 Stride 调度。`spawn` 根据程序名加载 ELF，创建子进程并建立父子关系。每个 TCB 保存优先级与 stride，调度器选择 stride 最小的 Ready 进程并按优先级记账。同时迁移了 `get_time`、`mmap`、`munmap`，通过 Ch5 全部本地测例。

## 一、如何把 CPU 调度变成工程问题

### 1. 人的需求

本实验最初的问题是：单核 CPU 上有多个可运行进程时，如何让它们并发前进，并使长期获得的 CPU 时间与各自优先级大致成正比。

`priority` 不是 PID，也不是用户指定的运行秒数，而是相对权重。例如优先级 10 的进程，长期获得的 CPU 时间应约为优先级 5 进程的两倍。

### 2. 将需求转化为状态

为了实现“按优先级分配”，每个任务的 `TaskControlBlockInner` 保存两个状态：

- `priority`：用户请求的 CPU 权重，初始值为 16，必须大于等于 2。
- `stride`：任务过去获得调度机会的累计“虚拟账单”，初始值为 0。

这两个值描述的是某个具体任务，因此属于 TCB，而不是系统调用模块或全局临时变量。

### 3. 将数学策略定义成输入和输出

一次调度策略可以被视作黑箱：

```text
输入：当前 Ready 队列中所有任务的 priority 和 stride
输出：下一次应该运行的 TaskControlBlock

规则：
1. 选择 stride 最小的任务 P；
2. 计算 pass = BIG_STRIDE / P.priority；
3. 更新 P.stride += pass；
4. 把 P 交给已有上下文切换机制运行。
```

优先级越高，`pass` 越小，账单增长越慢，因此该进程会更快再次成为最小值并被选中。

### 4. 找到工程决策点

最初我把决策点称为“run next task”。在当前工程中，更精确的位置是 `TaskManager::fetch()`：

```text
时钟中断 / yield / wait / exit
→ 当前任务交出 CPU
→ schedule / run_tasks
→ fetch_task
→ TaskManager::fetch
→ 选择最小 stride 的 Ready 任务
→ Processor 与 __switch 完成真正的上下文切换
```

因此本实验只替换“下一个任务是谁”的策略，没有重写时钟中断、上下文保存或 `__switch`。

## 二、工程修改

| 位置 | 修改内容 | 工程职责 |
| --- | --- | --- |
| `os/src/task/task.rs` | 在 TCB 中加入 `priority`、`stride`；增加设置优先级、读取账单和记录调度的方法；实现 `spawn` 子任务构造 | 保存单个进程拥有的状态，并提供受控接口 |
| `os/src/task/manager.rs` | 扫描 Ready 队列，选择最小 `stride` 的任务，并在返回前完成记账 | 实现调度策略决策点 |
| `os/src/syscall/process.rs` | 实现 `sys_spawn`、`sys_set_priority`，迁移 `get_time`、`mmap`、`munmap` | 解释用户参数、连接内核对象、返回结果 |
| `os/src/mm/memory_set.rs` | 迁移受检映射建立、解除映射和区域拆分 | 维持每个进程地址空间的真实状态 |
| `os/src/mm/page_table.rs` | 迁移跨页安全写回用户内存 | 支持 `get_time` 向用户地址写回 `TimeVal` |

代码中的块级变更注释记录了各逻辑块的修改原因、输入、输出、作用和抽象位置。

## 三、前向兼容与迁移

Ch5 的 Stride 测试使用 `get_time` 判断四秒是否结束。第一次运行时，调度代码已经能够编译，但 Ch5 分支中的 `sys_get_time` 仍是返回 `-1` 的桩函数，导致测试循环无法结束。这说明前向兼容不是简单复制文件，而是恢复旧功能的外部契约并适配新的状态所有者。

Ch4 的实现通过旧任务结构定位地址空间；Ch5 中地址空间属于 `Arc<TaskControlBlock>`。因此迁移后的链路是：

```text
sys_mmap / sys_munmap
→ current_task()
→ TaskControlBlock::mmap / munmap
→ 当前任务 MemorySet
```

没有直接覆盖旧版 `task` 文件，从而保留 Ch5 的父子进程、Ready 队列和上下文切换结构。

## 四、测试设计与结果

### 测试 1：固定时间内的运行比例

**目的**：证明优先级不仅被写入 TCB，而且真正影响 CPU 分配。

**方法**：`ch5_stride` 同时创建六个计算逻辑相同的进程，优先级分别为 5、6、7、8、9、10。每个进程运行约四秒并统计循环次数，最后比较 `runtimes / priority`。

**实际结果**：

```text
priority 5  → ratio 5554960
priority 6  → ratio 5745800
priority 7  → ratio 5660171
priority 8  → ratio 5480450
priority 9  → ratio 5497644
priority 10 → ratio 5585600
```

最大值与最小值之比约为 1.05，小于实验要求的 1.5。结果说明运行次数随优先级增加，而归一化后的比例基本接近。

### 测试 2：改变优先级比例观察结果

官方六个子程序已经构成该对照实验：它们只改变优先级，运行逻辑和观察时间保持一致。若只观察优先级 5 与 10：

```text
预期：priority 10 的运行次数约为 priority 5 的两倍。
实际：56828000 / 27550000 ≈ 2.06（一次单项运行结果）。
```

这证明调度结果会随优先级比例变化，而不是所有任务平均分配时间。若后续自行把优先级改为 4 与 12，理论预测应接近 1:3；该预测尚未作为独立新测例运行，因此不把它写成实际结果。

### 其他功能与回归测试

- `ch5_setprio`：合法优先级返回原值，0、1 和负数返回 `-1`。
- `ch5_spawn0`：创建并回收 40 个子进程，第 41 次 `wait` 必须失败。
- `ch5_spawn1`：`waitpid` 能按 PID 取得对应退出码。
- `ch4_mmap*`、`ch4_unmap*`：迁移的地址空间功能继续通过；只读写入与不可读访问按预期产生 PageFault。
- 完整运行 `ch5_usertest`，最终输出 `ch5 Usertests passed!`。

这些测试证明当前输入范围内的外部行为符合契约，但不能证明数值永不溢出、多核环境正确或长期运行绝不发生饥饿。

## 五、问答作业：Stride 算法深入

### 1. 8 位无符号整数溢出后，实际会选择谁？

两个进程的 `pass` 都是 10：

```text
p1.stride = 255
p2.stride = 250
```

选择 p2 并增加 10 后，8 位无符号整数发生回绕：

```text
p2.stride = (250 + 10) mod 256 = 4
```

若使用普通无符号整数比较，4 小于 255，调度器会错误地再次选择 p2。按照没有溢出时的逻辑，p2 的真实累计值相当于 260，大于 p1 的 255，因此下一次应该选择 p1。问题的原因是普通数值顺序不能表达环形计数器跨过最大值后的先后关系。

### 2. 为什么 `STRIDE_MAX - STRIDE_MIN <= BigStride / 2`？

在所有任务初始 `stride` 相同、没有中途加入新任务的前提下，可以用不变量说明。

每次选择当前最小的 stride，设为 `m`。由于 `priority >= 2`：

```text
pass = BigStride / priority <= BigStride / 2
```

被选中的任务更新后最多变成 `m + BigStride / 2`；其他任务没有变化。若更新前所有任务都位于 `[m, m + BigStride / 2]`，更新后仍不会超过这个上界，而新的最小值不会小于原来的 `m`。初始时所有 stride 都为 0，差值为 0，因此该性质可以在每次调度后持续保持。

这个界限的重要意义是：任意两个合法 stride 在环上最多相距半圈，所以即使发生回绕，也能唯一判断谁在前、谁在后，不会出现两种解释都合理的情况。

### 3. 如何设计支持溢出的比较器？

先定义环形计数器的逻辑顺序。假设两个值永远不相等，并且它们之间的真实距离小于半个 `u64` 取值范围：

```rust
use core::cmp::Ordering;

const HALF_RANGE: u64 = 1u64 << 63;

struct Stride(u64);

impl PartialOrd for Stride {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let distance = self.0.wrapping_sub(other.0);
        let logical_order = if distance < HALF_RANGE {
            Ordering::Greater
        } else {
            Ordering::Less
        };

        // BinaryHeap 弹出“最大值”，因此反转逻辑顺序，
        // 让逻辑上最小的 stride 成为堆中的最大元素。
        Some(logical_order.reverse())
    }
}

impl PartialEq for Stride {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
```

题目提示中的 8 位例子描述的是反转前的环形逻辑顺序：`125 - 255` 回绕后的距离是 126，小于半圈 128，因此 125 被认为在 255 之后，`125 < 255` 为假；`129 - 255` 回绕后的距离是 130，大于半圈，因此 129 被认为在 255 之前，`129 < 255` 为真。

如果使用 `BinaryHeap<Reverse<Stride>>`，则 `PartialOrd` 可以直接返回 `logical_order`，不需要最后的 `reverse()`。实际 Rust 代码还应提供与 `PartialOrd` 一致的 `Ord`/`Eq` 实现。

本实验实际实现使用线性扫描而不是 `BinaryHeap`，并使用 `u64` 和 `saturating_add` 避免当前测例运行期间发生回绕。该实现满足本实验时间范围，但达到饱和值后会退化，不能替代上述严格的环形比较方案。

## 六、实验体会

本实验最重要的学习不是记住 Stride 公式，而是把“按优先级分配 CPU”变成工程问题：先定义可观察目标，再把所需历史状态放入 TCB，明确调度策略的输入和输出，在 `TaskManager::fetch` 这一决策点使用状态，最后用固定时间内的运行比例验证效果。

这形成了一条可复用的设计路径：

```text
人的需求
→ 可保存的状态
→ 明确的状态更新规则
→ 正确的状态所有者和决策点
→ 能区分真实效果与伪造返回值的测试
```

## 七、荣誉准则

1. 在完成本次实验的过程（含此前学习过程）中，我曾与 **OpenAI Codex** 就以下方面进行交流，代码中对应位置的块级变更注释记录了相关设计内容：

   - 讨论并实现了 `spawn` 的程序名到 ELF、TCB、父子关系和 Ready 队列调用链。
   - 讨论并实现了 `priority/stride` 的状态所有权、Stride 调度决策点、系统调用接口、Rust 编译问题和测试方法。
   - 协助将本人 Ch4 中已经完成的 `get_time`、`mmap`、`munmap` 适配到 Ch5 的进程结构，并运行 Docker/QEMU 测试。
   - 根据本人给出的学习抽象和报告主线，协助整理本实验报告及块级变更注释。

2. 此外，我参考了以下资料：

   - LearningOS《rCore-Tutorial-Guide》第五章实验指导、问答作业与荣誉准则。
   - 本仓库 `README.md`、`os/src/task/`、`os/src/syscall/`、`os/src/mm/` 源码。
   - `rCore-Tutorial-Test` 中的 `ch5_spawn0`、`ch5_spawn1`、`ch5_setprio`、`ch5_stride` 及前向兼容测试。

3. 除上述明确披露的 AI 辅助范围外，其余学习、理解、选择和提交决定由我本人完成。我清楚地知道，从上述交流获得的信息在一定程度上降低了实验难度，并可能影响评分。

4. 我未从其他同学或未注明来源的公开实现中复制实验代码，也不会向他人公开本实验代码。我有义务妥善保管提交内容，并保证提交代码无意破坏或妨碍任何计算机系统的正常运行。我清楚课程关于学术诚信的要求及违反规定的后果。
