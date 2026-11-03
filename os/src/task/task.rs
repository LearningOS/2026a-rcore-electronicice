//! Types related to task management & Functions for completely changing TCB
use super::TaskContext;
use super::{kstack_alloc, pid_alloc, KernelStack, PidHandle};
use crate::config::TRAP_CONTEXT_BASE;
use crate::mm::{MapPermission, MemorySet, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use crate::trap::{trap_handler, TrapContext};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefMut;

// ==================== 修改块开始：Stride 调度参数 ====================
// 修改内容：定义新任务默认 CPU 权重和 Stride 记账计算基数。
// 修改原因：每个任务必须有确定的初始调度状态，且整数除法需要足够大的公共基数降低误差。
// 上下文：任务构造时使用 DEFAULT_PRIORITY，被调度时使用 BIG_STRIDE 计算 pass。
// 输入：编译期调度策略常量，无运行时输入。
// 输出：为任务初始化和单次调度记账提供统一参数。
// 作用：固定本实验调度策略的默认权重与数学尺度。
// 抽象位置：Stride 调度策略的参数层。
const DEFAULT_PRIORITY: usize = 16;
const BIG_STRIDE: u64 = 1_000_000_000;
// ==================== 修改块结束：Stride 调度参数 ====================

/// Task control block structure
///
/// Directly save the contents that will not change during running
pub struct TaskControlBlock {
    // Immutable
    /// Process identifier
    pub pid: PidHandle,

    /// Kernel stack corresponding to PID
    pub kernel_stack: KernelStack,

    /// Mutable
    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    /// Get the mutable reference of the inner TCB
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// Get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        let inner = self.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    /// The physical page number of the frame where the trap context is placed
    pub trap_cx_ppn: PhysPageNum,

    /// Application data can only appear in areas
    /// where the application address space is lower than base_size
    pub base_size: usize,

    /// Save task context
    pub task_cx: TaskContext,

    /// Maintain the execution status of the current process
    pub task_status: TaskStatus,

    /// Application address space
    pub memory_set: MemorySet,

    /// Parent process of the current process.
    /// Weak will not affect the reference count of the parent
    pub parent: Option<Weak<TaskControlBlock>>,

    /// A vector containing TCBs of all child processes of the current process
    pub children: Vec<Arc<TaskControlBlock>>,

    /// It is set when active exit or execution error occurs
    pub exit_code: i32,

    // ==================== 修改块开始：任务调度状态 ====================
    // 修改内容：为每个任务保存 CPU 权重和累计 Stride 两项调度状态。
    // 修改原因：公平分配必须同时知道用户请求的相对份额和任务过去获得服务的累计进度。
    // 上下文：由 set_priority 修改，由 TaskManager 选择任务时读取和更新。
    // 输入：任务创建默认值、用户优先级请求和每次被调度事件。
    // 输出：形成随任务生命周期保存、彼此隔离的调度状态。
    // 作用：让调度策略所需的数据归属于被调度的具体任务。
    // 抽象位置：进程控制块中的调度状态所有者。
    pub priority: usize,

    /// Accumulated scheduling progress
    pub stride: u64,
    // ==================== 修改块结束：任务调度状态 ====================

    /// Heap bottom
    pub heap_bottom: usize,

    /// Program break
    pub program_brk: usize,
}

impl TaskControlBlockInner {
    /// get the trap context
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }
    /// get the user token
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }
}

impl TaskControlBlock {
    /// Create a new process
    ///
    /// At present, it is only used for the creation of initproc
    pub fn new(elf_data: &[u8]) -> Self {
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        // push a task context which goes to trap_return to the top of kernel stack
        let task_control_block = Self {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    base_size: user_sp,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    // ==================== 修改块开始：新任务调度初始化 ====================
                    // 修改内容：为 ELF 新建任务初始化默认优先级和零累计账单。
                    // 修改原因：未调用 set_priority 的新进程也必须立即具备可比较的调度状态。
                    // 上下文：TaskControlBlock::new 同时服务 initproc 和 spawn 创建的新进程。
                    // 输入：编译期默认优先级和“尚未被调度”的初始事实。
                    // 输出：priority 为 16、stride 为 0 的 Ready 任务。
                    // 作用：建立新任务进入 Ready 队列前的调度不变量。
                    // 抽象位置：任务生命周期的初始化阶段。
                    priority: DEFAULT_PRIORITY,
                    stride: 0,
                    // ==================== 修改块结束：新任务调度初始化 ====================
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                })
            },
        };
        // prepare TrapContext in user space
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        task_control_block
    }

    /// Load a new elf to replace the original application address space and start execution
    pub fn exec(&self, elf_data: &[u8]) {
        // memory_set with elf program headers/trampoline/trap context/user stack
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();

        // **** access current TCB exclusively
        let mut inner = self.inner_exclusive_access();
        // substitute memory_set
        inner.memory_set = memory_set;
        // update trap_cx ppn
        inner.trap_cx_ppn = trap_cx_ppn;
        // initialize base_size
        inner.base_size = user_sp;
        // initialize trap_cx
        let trap_cx = inner.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            self.kernel_stack.get_top(),
            trap_handler as usize,
        );
        // **** release inner automatically
    }

    /// parent process fork the child process
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        // ---- access parent PCB exclusively
        let mut parent_inner = self.inner_exclusive_access();
        // copy user space(include trap context)
        let memory_set = MemorySet::from_existed_user(&parent_inner.memory_set);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // alloc a pid and a kernel stack in kernel space
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        let task_control_block = Arc::new(TaskControlBlock {
            pid: pid_handle,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    base_size: parent_inner.base_size,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    // ==================== 修改块开始：Fork 子任务调度初始化 ====================
                    // 修改内容：为 fork 产生的子任务建立独立的默认调度状态。
                    // 修改原因：子任务必须拥有自己的账单，不能与父任务共享或遗漏调度字段。
                    // 上下文：TaskControlBlock::fork 已完成地址空间复制并准备创建新的 Ready 任务。
                    // 输入：默认优先级和新子任务尚未被调度的初始状态。
                    // 输出：priority 为 16、stride 为 0 的子任务 TCB。
                    // 作用：让 fork 子任务以完整且独立的调度状态加入系统。
                    // 抽象位置：子进程创建过程中的调度状态初始化阶段。
                    priority: DEFAULT_PRIORITY,
                    stride: 0,
                    // ==================== 修改块结束：Fork 子任务调度初始化 ====================
                    heap_bottom: parent_inner.heap_bottom,
                    program_brk: parent_inner.program_brk,
                })
            },
        });
        // add child
        parent_inner.children.push(task_control_block.clone());
        // modify kernel_sp in trap_cx
        // **** access child PCB exclusively
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top;
        // return
        task_control_block
        // **** release child PCB
        // ---- release parent PCB
    }

    // ==================== 修改块开始：Spawn 子任务构造 ====================
    // 修改内容：增加从 ELF 直接创建子任务并建立双向父子关系的 TCB 方法。
    // 修改原因：spawn 需要新地址空间，但仍必须纳入 wait 所依赖的父子生命周期管理。
    // 上下文：由 sys_spawn 在完成程序名到 ELF 查找后调用；加入 Ready 队列由调用者负责。
    // 输入：父任务 Arc 和目标用户程序 ELF 字节。
    // 输出：返回拥有新 PID、独立地址空间且已记录父子关系的子任务 Arc。
    // 作用：把“创建任务对象”和“登记家庭关系”封装成进程状态所有者的操作。
    // 抽象位置：进程控制块中的子进程构造机制。
    /// Create a child process directly from an ELF image.
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Arc<Self> {
        //创建一个子进程
        let child = Arc::new(TaskControlBlock::new(elf_data));

        {
            //通过一个变量和探针，去修改父进程的子进程列表
            let mut child_inner = child.inner_exclusive_access();
            child_inner.parent = Some(Arc::downgrade(self));
        }

        {
            //去修改子进程列表
            let mut parent_inner = self.inner_exclusive_access();
            parent_inner.children.push(child.clone());
        }

        child
    }
    // ==================== 修改块结束：Spawn 子任务构造 ====================

    /// get pid of process
    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    // ==================== 修改块开始：任务优先级修改 ====================
    // 修改内容：增加当前任务优先级的合法性检查和状态更新接口。
    // 修改原因：系统调用层不应直接操作 TCB 内部字段，且题目要求 priority 至少为 2。
    // 上下文：由 sys_set_priority 调用，更新值供后续 Stride pass 计算使用。
    // 输入：用户请求的有符号优先级。
    // 输出：合法时保存并原值返回；非法时不修改状态并返回 -1。
    // 作用：维护 priority 的取值不变量并封装对任务内部状态的修改权限。
    // 抽象位置：进程控制块的公开状态修改接口。
    /// Set the CPU allocation weight of this process.
    pub fn set_priority(&self, priority: isize) -> isize {
        if priority < 2 {
            return -1;
        }
        self.inner_exclusive_access().priority = priority as usize;
        priority
    }
    // ==================== 修改块结束：任务优先级修改 ====================

    // ==================== 修改块开始：任务调度账单接口 ====================
    // 修改内容：增加读取累计账单和记录一次被调度事件的 TCB 接口。
    // 修改原因：TaskManager 需要比较和更新调度状态，但不应知道 TCB 内部借用与字段细节。
    // 上下文：TaskManager 在选择候选任务时读取 stride，选中后调用记账方法。
    // 输入：当前任务已保存的 priority 和 stride。
    // 输出：读取接口返回 stride；记账接口按 pass 增加 stride 且保证 pass 至少为 1。
    // 作用：把调度状态的读写封装成“查看账单”和“记录一次服务”两个业务动作。
    // 抽象位置：任务状态所有者与调度策略之间的公开接口层。
    /// Return the accumulated scheduling progress of this process.
    pub fn scheduling_stride(&self) -> u64 {
        self.inner_exclusive_access().stride
    }

    /// Record that this process has been selected to run once.
    pub fn record_scheduled_once(&self) {
        let mut inner = self.inner_exclusive_access();
        let pass = (BIG_STRIDE / inner.priority as u64).max(1);
        inner.stride = inner.stride.saturating_add(pass);
    }
    // ==================== 修改块结束：任务调度账单接口 ====================

    // ==================== 修改块开始：任务地址空间接口 ====================
    // 修改内容：增加当前任务建立和解除用户映射的两个 TCB 接口。
    // 修改原因：Ch5 的地址空间属于各自 TaskControlBlock，旧 Ch4 行为必须适配新的状态所有者。
    // 上下文：分别由 sys_mmap 和 sys_munmap 调用，下游委托当前任务的 MemorySet 执行。
    // 输入：映射半开区间及权限，或需要解除的半开区间。
    // 输出：底层操作成功返回 true，冲突、缺失或非法区间返回 false。
    // 作用：把系统调用请求路由到正确进程的地址空间，同时隐藏 TCB 内部访问方式。
    // 抽象位置：进程状态所有者提供的虚拟内存公开接口层。
    /// Map a framed area into this process's address space.
    pub fn mmap(&self, start: usize, end: usize, permission: MapPermission) -> bool {
        self.inner_exclusive_access()
            .memory_set
            .insert_framed_area_checked(start.into(), end.into(), permission)
    }

    /// Remove a framed area from this process's address space.
    pub fn munmap(&self, start: usize, end: usize) -> bool {
        self.inner_exclusive_access()
            .memory_set
            .remove_framed_area_checked(start.into(), end.into())
    }
    // ==================== 修改块结束：任务地址空间接口 ====================

    /// change the location of the program break. return None if failed.
    pub fn change_program_brk(&self, size: i32) -> Option<usize> {
        let mut inner = self.inner_exclusive_access();
        let heap_bottom = inner.heap_bottom;
        let old_break = inner.program_brk;
        let new_brk = inner.program_brk as isize + size as isize;
        if new_brk < heap_bottom as isize {
            return None;
        }
        let result = if size < 0 {
            inner
                .memory_set
                .shrink_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        } else {
            inner
                .memory_set
                .append_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        };
        if result {
            inner.program_brk = new_brk as usize;
            Some(old_break)
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
/// task status: UnInit, Ready, Running, Exited
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Zombie,
}
