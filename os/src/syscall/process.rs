//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    config::PAGE_SIZE,
    loader::get_app_data_by_name,
    mm::{copy_to_user, translated_refmut, translated_str, MapPermission},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
// ==================== 修改块开始：时间系统调用 ====================
// 修改内容：实现取得内核时间并安全写回用户 TimeVal 的系统调用。
// 修改原因：睡眠测试和 Stride 公平性测试需要可靠的时间来源，并要求处理跨页用户地址。
// 上下文：由用户库 get_time 调用，依赖 timer::get_time_us 和 copy_to_user。
// 输入：用户态 TimeVal 指针；时区参数在本实验中不使用。
// 输出：成功写入秒和微秒返回 0；用户地址无效返回 -1。
// 作用：把内核微秒计数转换成 ABI 约定的结构体并安全送回当前进程。
// 抽象位置：时间服务的系统调用入口与用户内存适配层。
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time", current_task().unwrap().pid.0);
    let us = get_time_us();
    let time = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &time as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    if copy_to_user(current_user_token(), ts as usize, bytes) {
        0
    } else {
        -1
    }
}
// ==================== 修改块结束：时间系统调用 ====================

/// YOUR JOB: Implement mmap.
// ==================== 修改块开始：内存映射系统调用 ====================
// 修改内容：实现 mmap 参数检查、权限转换和当前任务地址空间映射。
// 修改原因：Ch4/Ch5 前向兼容测试要求合法请求成功、重复或非法请求返回错误而不破坏内核。
// 上下文：由用户态 mmap 进入，向当前 TaskControlBlock 的 mmap 方法提交受检请求。
// 输入：页对齐起始地址、字节长度和 R/W/X 权限位。
// 输出：映射成功返回 0；参数、溢出或地址冲突返回 -1。
// 作用：把用户 ABI 参数转换成内核 MapPermission 和明确的地址区间。
// 抽象位置：虚拟内存服务的系统调用参数适配层。
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel:pid[{}] sys_mmap", current_task().unwrap().pid.0);
    if start % PAGE_SIZE != 0 || len == 0 || prot == 0 || prot & !0x7 != 0 {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    let mut permission = MapPermission::U;
    if prot & 0x1 != 0 {
        permission |= MapPermission::R;
    }
    if prot & 0x2 != 0 {
        permission |= MapPermission::W;
    }
    if prot & 0x4 != 0 {
        permission |= MapPermission::X;
    }
    if current_task().unwrap().mmap(start, end, permission) {
        0
    } else {
        -1
    }
}
// ==================== 修改块结束：内存映射系统调用 ====================

/// YOUR JOB: Implement munmap.
// ==================== 修改块开始：解除映射系统调用 ====================
// 修改内容：实现 munmap 参数检查和当前任务映射区间释放。
// 修改原因：用户必须能够撤销已有映射，并对未映射或非法地址得到稳定错误结果。
// 上下文：由用户态 munmap 进入，向当前 TaskControlBlock 的 munmap 方法提交地址区间。
// 输入：页对齐起始地址和需要解除映射的字节长度。
// 输出：解除成功或零长度请求返回 0；非法、溢出或未映射区间返回 -1。
// 作用：把用户给出的 start/len 转换成安全的半开区间并请求 MemorySet 删除。
// 抽象位置：虚拟内存释放服务的系统调用参数适配层。
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_munmap", current_task().unwrap().pid.0);
    if start % PAGE_SIZE != 0 {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return -1,
    };
    if current_task().unwrap().munmap(start, end) {
        0
    } else {
        -1
    }
}
// ==================== 修改块结束：解除映射系统调用 ====================

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
// ==================== 修改块开始：Spawn 系统调用 ====================
// 修改内容：实现通过程序名直接创建子进程并加入调度队列的 spawn 系统调用。
// 修改原因：Ch5 测试要求父进程获得真实子 PID，且子进程能够运行、退出并被 wait 回收。
// 上下文：由用户态 spawn 调用，连接 loader、父任务 TCB 和全局 Ready 队列。
// 输入：用户地址空间中以零结尾的程序名字符串。
// 输出：成功返回新子进程 PID；程序不存在返回 -1。
// 作用：把用户程序名转换成 ELF，再创建受父进程管理且可被调度的任务对象。
// 抽象位置：进程创建服务的系统调用编排层。
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);

    let token = current_user_token();
    let path = translated_str(token, path);
    let elf_data = match get_app_data_by_name(path.as_str()) {
        Some(data) => data,
        None => return -1,
    };

    let parent = current_task().unwrap();
    let child = parent.spawn(elf_data);
    let child_pid = child.pid.0;
    add_task(child);
    child_pid as isize
}
// ==================== 修改块结束：Spawn 系统调用 ====================

// YOUR JOB: Set task priority.
// ==================== 修改块开始：设置优先级系统调用 ====================
// 修改内容：实现修改当前进程 CPU 权重的 set_priority 系统调用。
// 修改原因：Stride 测试需要用户进程声明相对 CPU 份额，并拒绝小于 2 的非法权重。
// 上下文：由用户态 set_priority 调用，具体状态检查和保存委托给当前 TaskControlBlock。
// 输入：用户请求的有符号优先级数值。
// 输出：合法时保存并返回该优先级；非法时保持原状态并返回 -1。
// 作用：把用户调度需求传递给拥有 priority 状态的当前任务。
// 抽象位置：调度策略的系统调用入口层。
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority [{}]",
        current_task().unwrap().pid.0,
        prio
    );
    current_task().unwrap().set_priority(prio)
}
// ==================== 修改块结束：设置优先级系统调用 ====================
