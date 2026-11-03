//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use lazy_static::*;
///A array of `TaskControlBlock` that is thread-safe
pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A stride scheduler.
impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    /// Add process back to ready queue
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    // ==================== 修改块开始：最小 Stride 候选选择 ====================
    // 修改内容：增加在 Ready 队列中线性寻找最小 Stride 任务的选择函数。
    // 修改原因：Stride 调度要求每次从所有可运行任务中选择累计账单最小者。
    // 上下文：仅由 TaskManager::fetch 调用，候选集合由 ready_queue 提供。
    // 输入：当前 Ready 队列及其中每个任务保存的 scheduling_stride。
    // 输出：队列非空时返回选中任务下标；队列为空返回 None。
    // 作用：把调度策略“选择最小账单”翻译成明确的候选位置，不执行任务切换。
    // 抽象位置：任务调度的策略选择层。
    fn find_smallest_stride_task_index(&self) -> Option<usize> {
        let first_task = self.ready_queue.front()?;
        let mut selected_index = 0;
        let mut smallest_stride = first_task.scheduling_stride();

        for (index, task) in self.ready_queue.iter().enumerate().skip(1) {
            let task_stride = task.scheduling_stride();
            if task_stride < smallest_stride {
                selected_index = index;
                smallest_stride = task_stride;
            }
        }

        Some(selected_index)
    }
    // ==================== 修改块结束：最小 Stride 候选选择 ====================

    // ==================== 修改块开始：Stride 任务出队 ====================
    // 修改内容：用 Stride 选择替代 FIFO 出队，并在任务运行前记录一次调度账单。
    // 修改原因：仅保存 priority 不会改变 CPU 份额，调度出口必须实际使用该策略。
    // 上下文：由 fetch_task 调用，返回值继续交给 Processor 和已有 __switch 机制运行。
    // 输入：Ready 队列和最小 Stride 选择结果。
    // 输出：返回已从队列移除且更新过账单的任务；无任务时返回 None。
    // 作用：完成“选择候选者—移出队列—记录本次服务”这一原子调度步骤。
    // 抽象位置：调度策略与既有上下文切换机制之间的连接层。
    /// Select a ready process according to the stride policy.
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        let selected_index = self.find_smallest_stride_task_index()?;
        let selected_task = self.ready_queue.remove(selected_index)?;
        selected_task.record_scheduled_once();
        Some(selected_task)
    }
    // ==================== 修改块结束：Stride 任务出队 ====================
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
