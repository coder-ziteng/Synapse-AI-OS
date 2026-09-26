//! `Scheduler` 门面：组合 ThreadTable + RunQueue + SleepQueue，
//! 实现线程生命周期 API 与调度决策（纯逻辑，无硬件操作）。
//!
//! ## 抢占模型（需求评审修订，P3-T6 内核接线依此消费）
//!
//! - `tick(now)`：时间片到期只置 `need_resched` 标志，**不切换**。
//! - `schedule(now)`：返回 [`SwitchDecision`]，由内核在**中断返回边界**
//!   或**阻塞点**调用并执行真正的 switch_to（P3-T5）。
//! - 高优先级线程入队（spawn/unblock/wake）会置位 `need_resched`，
//!   当前线程在下一个检查点被抢占——单核 MVP 下这是唯一的抢占触发源。

use crate::runqueue::RunQueue;
use crate::sleepq::SleepQueue;
use crate::tcb::ThreadTable;
use crate::types::{SchedError, ThreadId, ThreadState};

/// 默认时间片（tick 数）。100Hz PIT 下 = 20ms，与主流 OS 时间片量级一致。
pub const DEFAULT_TIME_SLICE: u64 = 2;

/// 调度决策（`schedule()` 的返回值，内核据此执行上下文切换）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwitchDecision {
    /// 无需切换：继续运行当前线程（无就绪线程或当前仍是最优）。
    KeepCurrent,
    /// 切换到 `next`。`prev` 为被换出的线程（None = 首次调度/无当前线程）。
    Switch { prev: Option<ThreadId>, next: ThreadId },
    /// 无当前线程且无就绪线程：内核应 idle（hlt 等待下一个中断）。
    Idle,
}

/// 调度器（纯逻辑）。内核集成层用 SpinLock 包装后作为全局单例（MVP-3 单线程）。
#[derive(Clone, Debug)]
pub struct Scheduler {
    threads: ThreadTable,
    rq: RunQueue,
    sq: SleepQueue,
    current: Option<ThreadId>,
    need_resched: bool,
    /// 当前线程时间片截止时刻（tick 时间基）；0 = 无当前线程。
    slice_deadline: u64,
    time_slice: u64,
    /// 总切换次数（smoke/FR8 观测用）。
    switch_count: u64,
}

impl Scheduler {
    /// 以默认时间片创建空调度器。
    pub fn new() -> Self {
        Self::with_time_slice(DEFAULT_TIME_SLICE)
    }

    /// 以指定时间片（tick 数，≥1）创建空调度器。
    pub fn with_time_slice(time_slice: u64) -> Self {
        Scheduler {
            threads: ThreadTable::new(),
            rq: RunQueue::new(),
            sq: SleepQueue::new(),
            current: None,
            need_resched: false,
            slice_deadline: 0,
            time_slice: time_slice.max(1),
            switch_count: 0,
        }
    }

    // ====================================================================
    // 线程生命周期
    // ====================================================================

    /// 创建线程（Ready 入就绪队列）。`owner_pid` 为 FR8 记账透传（0 = 内核线程）。
    /// 若新线程优先级高于当前线程 → 置 `need_resched`（下个检查点抢占）。
    pub fn spawn(&mut self, priority: u8, owner_pid: u32) -> Result<ThreadId, SchedError> {
        if priority as usize >= crate::types::NQ {
            return Err(SchedError::InvalidArg);
        }
        let id = self.threads.alloc(crate::types::Priority(priority), owner_pid)?;
        self.rq.push(priority, id)?;
        if let Some(cur) = self.current {
            if let Ok(t) = self.threads.get(cur) {
                if priority < t.priority.0 {
                    self.need_resched = true; // 高优先级到达 → 请求抢占
                }
            }
        }
        Ok(id)
    }

    /// 当前线程主动让出（yield）：Running → Ready 入队尾，置 need_resched。
    pub fn yield_current(&mut self) -> Result<(), SchedError> {
        let cur = self.current.ok_or(SchedError::BadState)?;
        self.threads.transition(cur, ThreadState::Ready)?;
        let prio = self.threads.get(cur)?.priority.0;
        self.rq.push(prio, cur)?;
        self.current = None;
        self.need_resched = true;
        Ok(())
    }

    /// 阻塞当前线程（锁/IPC 等待）：Running → Blocked，摘出 CPU。
    pub fn block_current(&mut self) -> Result<(), SchedError> {
        let cur = self.current.ok_or(SchedError::BadState)?;
        self.threads.transition(cur, ThreadState::Blocked)?;
        self.current = None;
        self.need_resched = true;
        Ok(())
    }

    /// 唤醒阻塞线程：Blocked → Ready 入队；若优先级高于当前线程则请求抢占。
    pub fn unblock(&mut self, id: ThreadId) -> Result<(), SchedError> {
        self.threads.transition(id, ThreadState::Ready)?;
        let prio = self.threads.get(id)?.priority.0;
        self.rq.push(prio, id)?;
        self.maybe_preempt(prio);
        Ok(())
    }

    /// 当前线程定时睡眠：Running → Sleeping，deadline 为绝对 tick 时刻。
    pub fn sleep_current(&mut self, deadline: u64) -> Result<(), SchedError> {
        let cur = self.current.ok_or(SchedError::BadState)?;
        self.threads.transition(cur, ThreadState::Sleeping)?;
        if let Ok(t) = self.threads.get_mut(cur) {
            t.deadline = deadline;
        }
        self.sq.sleep(cur, deadline)?;
        self.current = None;
        self.need_resched = true;
        Ok(())
    }

    /// 每 tick 调用：唤醒所有到期睡眠线程（Sleeping → Ready），
    /// 并检查当前线程时间片。返回本次唤醒个数。
    pub fn tick(&mut self, now: u64, woken_out: &mut [ThreadId]) -> usize {
        let n = self.sq.wake_due(now, woken_out);
        for i in 0..n {
            let id = woken_out[i];
            // Sleeping → Ready（状态机保证合法；失败即内部 bug，忽略继续）
            if self.threads.transition(id, ThreadState::Ready).is_ok() {
                if let Ok(t) = self.threads.get(id) {
                    let prio = t.priority.0;
                    let _ = self.rq.push(prio, id);
                    self.maybe_preempt(prio);
                }
            }
        }
        // 时间片检查
        if self.current.is_some() && now >= self.slice_deadline {
            self.need_resched = true;
        }
        n
    }

    /// 冻结线程（FR10）：任意非终态 → Frozen，从就绪/睡眠队列摘除。
    /// 冻结当前线程会摘出 CPU 并置 need_resched。
    pub fn freeze(&mut self, id: ThreadId) -> Result<(), SchedError> {
        let was_ready = self.threads.get(id)?.state == ThreadState::Ready;
        let was_current = self.current == Some(id);
        self.threads.transition(id, ThreadState::Frozen)?;
        if was_ready {
            self.rq.remove(id);
        }
        if self.sq.cancel(id) {
            // 从睡眠队列摘除（deadline 保留在 TCB 供审计）
        }
        if was_current {
            self.current = None;
            self.need_resched = true;
        }
        Ok(())
    }

    /// 解冻线程（FR10）：Frozen → Ready 重新入队。
    pub fn thaw(&mut self, id: ThreadId) -> Result<(), SchedError> {
        self.threads.transition(id, ThreadState::Ready)?;
        let prio = self.threads.get(id)?.priority.0;
        self.rq.push(prio, id)?;
        self.maybe_preempt(prio);
        Ok(())
    }

    /// 退出线程：任意非终态 → Exited；若是当前线程则摘出 CPU。
    /// 槽位保留待 `reap`（与 proc 的 exit→reap 生命周期对齐）。
    pub fn exit(&mut self, id: ThreadId) -> Result<(), SchedError> {
        // 从队列摘除（按所处状态）
        let state = self.threads.get(id)?.state;
        match state {
            ThreadState::Ready => {
                self.rq.remove(id);
            }
            ThreadState::Sleeping => {
                self.sq.cancel(id);
            }
            _ => {}
        }
        self.threads.transition(id, ThreadState::Exited)?;
        if self.current == Some(id) {
            self.current = None;
            self.need_resched = true;
        }
        Ok(())
    }

    /// 回收已退出线程的槽位（generation +1 后可复用）。
    pub fn reap(&mut self, id: ThreadId) -> Result<(), SchedError> {
        self.threads.release(id)
    }

    // ====================================================================
    // 调度决策
    // ====================================================================

    /// 核心决策：返回是否需要切换及切换目标。**纯函数式**——除
    /// current/need_resched/slice_deadline/switch_count 外不改任何状态；
    /// 真正的上下文切换与状态迁移（prev → Ready 等）由内核在
    /// switch_to 成功后调用 [`commit_switch`] 完成。
    pub fn schedule(&mut self, now: u64) -> SwitchDecision {
        let next = match self.rq.peek() {
            Some(n) => n,
            None => {
                return if self.current.is_some() {
                    SwitchDecision::KeepCurrent
                } else {
                    SwitchDecision::Idle
                };
            }
        };

        let cur = self.current;
        // 当前线程仍是最优 → 不换：就绪队列中无严格更高优先级（best >= cur）、
        // 无抢占请求（need_resched）、时间片未到期。
        // 注：Running 线程不在就绪队列中，peek 即"换出它之后的下一个"。
        if let Some(c) = cur {
            if let Ok(t) = self.threads.get(c) {
                let preempted = self.rq.best_priority().map_or(false, |p| p < t.priority.0);
                if !preempted && !self.need_resched && now < self.slice_deadline {
                    return SwitchDecision::KeepCurrent;
                }
            }
            // get(c) 失败 = 内部不一致（current 指向失效槽位），落到切换路径自愈。
        }
        self.need_resched = false;
        self.slice_deadline = now.saturating_add(self.time_slice);
        self.switch_count += 1;
        SwitchDecision::Switch { prev: cur, next }
    }

    /// 内核 switch_to 成功后回调：迁移状态（prev Running → Ready 入队尾、
    /// next → Running）并更新 current。返回 Err 表示状态已非法（不应发生，
    /// 内核侧应 panic 留证）。
    pub fn commit_switch(&mut self, prev: Option<ThreadId>, next: ThreadId) -> Result<(), SchedError> {
        if let Some(p) = prev {
            if p != next {
                // 仅当 prev 仍是 Running（正常抢占/RR 换出）才回队；
                // block/sleep/exit/freeze 路径已自行摘出，状态非 Running。
                if self.threads.get(p)?.state == ThreadState::Running {
                    self.threads.transition(p, ThreadState::Ready)?;
                    let prio = self.threads.get(p)?.priority.0;
                    self.rq.push(prio, p)?;
                }
            }
        }
        // next 必须此刻仍在队首（schedule 与 commit 之间无并发修改——
        // 单核 + 内核集成层临界区保证；此断言防御集成层 bug）
        if self.rq.peek() != Some(next) {
            return Err(SchedError::BadState);
        }
        self.rq.pop();
        self.threads.transition(next, ThreadState::Running)?;
        self.current = Some(next);
        Ok(())
    }

    /// 取走 need_resched 标志（内核在中断返回边界/阻塞点检查后清零）。
    pub fn take_need_resched(&mut self) -> bool {
        core::mem::replace(&mut self.need_resched, false)
    }

    /// 只读查看 need_resched（不清零）。
    pub fn need_resched(&self) -> bool {
        self.need_resched
    }

    // ====================================================================
    // 观测（smoke / FR8 数据源）
    // ====================================================================

    /// 当前运行线程。
    pub fn current(&self) -> Option<ThreadId> {
        self.current
    }

    /// 线程状态查询。
    pub fn state_of(&self, id: ThreadId) -> Result<ThreadState, SchedError> {
        Ok(self.threads.get(id)?.state)
    }

    /// TCB 只读访问（内核层读 ctx / deadline / owner_pid）。
    pub fn thread(&self, id: ThreadId) -> Result<&crate::tcb::Tcb, SchedError> {
        self.threads.get(id)
    }

    /// TCB 可写访问（内核 P3-T5 switch_to 写 ctx 槽位）。
    pub fn thread_mut(&mut self, id: ThreadId) -> Result<&mut crate::tcb::Tcb, SchedError> {
        self.threads.get_mut(id)
    }

    /// 存活线程数（未 reap）。
    pub fn live_count(&self) -> usize {
        self.threads.live_count()
    }

    /// 就绪队列长度。
    pub fn ready_count(&self) -> usize {
        self.rq.len()
    }

    /// 睡眠队列长度。
    pub fn sleeping_count(&self) -> usize {
        self.sq.len()
    }

    /// 累计切换次数。
    pub fn switch_count(&self) -> u64 {
        self.switch_count
    }

    /// 最早睡眠 deadline（内核 idle 时决定 hlt 窗口用）。
    pub fn next_deadline(&self) -> Option<u64> {
        self.sq.next_deadline()
    }

    // ====================================================================
    // 内部
    // ====================================================================

    /// 新高优先级线程入队时的抢占判定。
    fn maybe_preempt(&mut self, new_prio: u8) {
        if let Some(cur) = self.current {
            if let Ok(t) = self.threads.get(cur) {
                if new_prio < t.priority.0 {
                    self.need_resched = true;
                }
            }
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcb::MAX_THREADS;

    /// 驱动一轮 schedule+commit（模拟内核中断返回边界的完整切换）。
    fn run_schedule(s: &mut Scheduler, now: u64) -> SwitchDecision {
        let d = s.schedule(now);
        if let SwitchDecision::Switch { prev, next } = d {
            s.commit_switch(prev, next).unwrap();
        }
        d
    }

    #[test]
    fn first_schedule_picks_spawned_thread() {
        let mut s = Scheduler::new();
        let a = s.spawn(2, 0).unwrap();
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Switch { prev: None, next: a });
        assert_eq!(s.current(), Some(a));
        assert_eq!(s.state_of(a), Ok(ThreadState::Running));
    }

    #[test]
    fn rr_rotation_on_slice_expiry() {
        let mut s = Scheduler::with_time_slice(2);
        let a = s.spawn(1, 0).unwrap();
        let b = s.spawn(1, 0).unwrap();
        let c = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0); // → a (slice_deadline=2)
        assert_eq!(s.current(), Some(a));
        // 时间片未到：KeepCurrent
        assert_eq!(s.schedule(1), SwitchDecision::KeepCurrent);
        // tick 到 2：时间片到期 → need_resched → 切 b
        s.tick(2, &mut []);
        assert!(s.need_resched());
        assert_eq!(run_schedule(&mut s, 2), SwitchDecision::Switch { prev: Some(a), next: b });
        assert_eq!(s.state_of(a), Ok(ThreadState::Ready)); // a 回队尾
        s.tick(4, &mut []);
        assert_eq!(run_schedule(&mut s, 4), SwitchDecision::Switch { prev: Some(b), next: c });
        s.tick(6, &mut []);
        assert_eq!(run_schedule(&mut s, 6), SwitchDecision::Switch { prev: Some(c), next: a }); // 轮回
        assert_eq!(s.switch_count(), 4);
    }

    #[test]
    fn priority_preemption() {
        let mut s = Scheduler::new();
        let low = s.spawn(5, 0).unwrap();
        run_schedule(&mut s, 0);
        assert_eq!(s.current(), Some(low));
        let high = s.spawn(0, 0).unwrap(); // 高优先级到达
        assert!(s.need_resched()); // 立即请求抢占
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Switch { prev: Some(low), next: high });
        assert_eq!(s.state_of(low), Ok(ThreadState::Ready));
    }

    #[test]
    fn block_unblock_cycle() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        let b = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0); // a running
        s.block_current().unwrap();
        assert_eq!(s.state_of(a), Ok(ThreadState::Blocked));
        assert_eq!(s.current(), None);
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Switch { prev: None, next: b });
        s.unblock(a).unwrap();
        assert_eq!(s.state_of(a), Ok(ThreadState::Ready));
        // b 时间片到期后轮回 a
        s.tick(100, &mut []);
        assert_eq!(run_schedule(&mut s, 100), SwitchDecision::Switch { prev: Some(b), next: a });
    }

    #[test]
    fn sleep_wake_by_deadline() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        s.sleep_current(50).unwrap();
        assert_eq!(s.state_of(a), Ok(ThreadState::Sleeping));
        assert_eq!(s.sleeping_count(), 1);
        // tick 49：未到期
        let mut out = [ThreadId(0); MAX_THREADS];
        assert_eq!(s.tick(49, &mut out), 0);
        assert_eq!(s.schedule(49), SwitchDecision::Idle); // 无就绪无当前 → Idle
        // tick 50：到期唤醒
        assert_eq!(s.tick(50, &mut out), 1);
        assert_eq!(out[0], a);
        assert_eq!(s.state_of(a), Ok(ThreadState::Ready));
        assert_eq!(run_schedule(&mut s, 50), SwitchDecision::Switch { prev: None, next: a });
    }

    #[test]
    fn freeze_thaw() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        let b = s.spawn(2, 0).unwrap();
        run_schedule(&mut s, 0); // a running
        s.freeze(a).unwrap(); // 冻结当前线程 → 摘出 CPU
        assert_eq!(s.state_of(a), Ok(ThreadState::Frozen));
        assert_eq!(s.current(), None);
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Switch { prev: None, next: b });
        // 冻结的不被调度：只有 b 可跑
        s.thaw(a).unwrap();
        assert_eq!(s.state_of(a), Ok(ThreadState::Ready));
        s.tick(100, &mut []); // b 时间片到期
        assert_eq!(run_schedule(&mut s, 100), SwitchDecision::Switch { prev: Some(b), next: a });
    }

    #[test]
    fn exit_and_reap_and_slot_reuse() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        s.exit(a).unwrap();
        assert_eq!(s.state_of(a), Ok(ThreadState::Exited));
        assert_eq!(s.current(), None);
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Idle);
        s.reap(a).unwrap();
        assert_eq!(s.live_count(), 0);
        let a2 = s.spawn(1, 0).unwrap();
        assert_ne!(a, a2); // 同槽位但 generation +1
        assert_eq!(s.state_of(a), Err(SchedError::NotFound)); // 旧 ID 失效
    }

    #[test]
    fn idle_when_nothing_runnable() {
        let mut s = Scheduler::new();
        assert_eq!(s.schedule(0), SwitchDecision::Idle);
        let a = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        s.block_current().unwrap();
        assert_eq!(s.schedule(0), SwitchDecision::Idle);
        assert_eq!(s.state_of(a), Ok(ThreadState::Blocked));
    }

    #[test]
    fn keep_current_when_nothing_better() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        // 时间片内再 schedule：KeepCurrent 且不重复计切换
        assert_eq!(s.schedule(0), SwitchDecision::KeepCurrent);
        assert_eq!(s.switch_count(), 1);
        assert_eq!(s.current(), Some(a));
    }

    #[test]
    fn yield_goes_to_queue_tail() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        let b = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0); // a
        s.yield_current().unwrap();
        assert_eq!(run_schedule(&mut s, 0), SwitchDecision::Switch { prev: None, next: b });
        s.tick(100, &mut []);
        assert_eq!(run_schedule(&mut s, 100), SwitchDecision::Switch { prev: Some(b), next: a });
    }

    #[test]
    fn invalid_priority_at_spawn() {
        let mut s = Scheduler::new();
        assert_eq!(s.spawn(crate::types::NQ as u8, 0), Err(SchedError::InvalidArg));
    }

    #[test]
    fn capacity_limit() {
        let mut s = Scheduler::new();
        for _ in 0..MAX_THREADS {
            s.spawn(3, 0).unwrap();
        }
        assert_eq!(s.spawn(3, 0), Err(SchedError::NoSpace));
        assert_eq!(s.live_count(), MAX_THREADS);
    }

    #[test]
    fn operations_on_stale_id_fail() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        s.exit(a).unwrap();
        s.reap(a).unwrap();
        assert_eq!(s.unblock(a), Err(SchedError::NotFound));
        assert_eq!(s.freeze(a), Err(SchedError::NotFound));
    }

    #[test]
    fn tick_wakes_multiple_in_order() {
        let mut s = Scheduler::new();
        let a = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        s.sleep_current(20).unwrap();
        let b = s.spawn(1, 0).unwrap();
        run_schedule(&mut s, 0);
        s.sleep_current(10).unwrap();
        let mut out = [ThreadId(0); MAX_THREADS];
        let n = s.tick(25, &mut out); // 两个都到期
        assert_eq!(n, 2);
        assert_eq!(out[0], b); // deadline 10 先
        assert_eq!(out[1], a); // deadline 20 后
        assert_eq!(s.ready_count(), 2);
    }
}
