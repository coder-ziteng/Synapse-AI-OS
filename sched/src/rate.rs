//! FR10 频率计数原语：per-process syscall/IPC 固定窗口计数器（`RateTable`）。
//!
//! 行为围栏的**数据源**（与 FR8 对 FR10 的关系同构）：内核在 syscall 入口
//! 与 IPC send 路径调用 `record_*`，超限置**粘性标志**（sticky over flag）；
//! 处置策略（freeze / 审计事件 / 降频警告）留给集成层与用户态监督树
//! （Doc 02 §5.4），本模块不做任何 enforcement。
//!
//! 固定窗口（非滑动）：MVP 取舍——O(1) 记账、零额外存储；窗口边界的
//! 2× 突发误差对"行为围栏"场景（抓数量级异常）可接受。

use crate::types::SchedError;

/// 频率计数槽位数（与 proc crate MAX_PROCS / [`crate::MAX_PROC_LEDGERS`] 对齐）。
pub const MAX_RATE_SLOTS: usize = 128;

/// 默认窗口长度（tick 时间基）。100Hz PIT 下 = 1 秒。
pub const DEFAULT_RATE_WINDOW: u64 = 100;

/// 默认 syscall 频率上限（次/窗口）。
pub const DEFAULT_SYSCALL_LIMIT: u32 = 10_000;

/// 默认 IPC 频率上限（次/窗口）。
pub const DEFAULT_IPC_LIMIT: u32 = 10_000;

/// 单维固定窗口计数器。
#[derive(Clone, Copy, Debug)]
struct WindowCounter {
    /// 当前窗口起点（tick 时间基）。
    window_start: u64,
    /// 窗口长度（tick）。
    window_len: u64,
    /// 本窗口已计数。
    count: u32,
    /// 上限（次/窗口）。
    limit: u32,
    /// 粘性超限标志：置位后保持，直到监督方 `clear_over`。
    over: bool,
}

impl WindowCounter {
    const fn new(window_len: u64, limit: u32) -> Self {
        WindowCounter { window_start: 0, count: 0, limit, over: false, window_len }
    }

    /// 记一次事件（now = 当前 tick）。窗口滚动惰性重置计数；
    /// 返回记录后是否处于超限状态。
    fn record(&mut self, now: u64) -> bool {
        // now < window_start（时间回退，集成层 bug）不滚动，只累加——防御性。
        if now >= self.window_start.saturating_add(self.window_len) {
            self.window_start = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        if self.count > self.limit {
            self.over = true;
        }
        self.over
    }
}

/// 单进程频率条目（syscall + IPC 两维独立）。
#[derive(Clone, Copy, Debug)]
struct RateEntry {
    pid: u32,
    syscall: WindowCounter,
    ipc: WindowCounter,
}

/// 固定容量进程频率表。非 const 构造（与其他表一致，集成层用锁包装）。
#[derive(Clone, Debug)]
pub struct RateTable {
    entries: [Option<RateEntry>; MAX_RATE_SLOTS],
    live: usize,
}

impl RateTable {
    /// 创建空表。
    pub fn new() -> Self {
        RateTable { entries: [None; MAX_RATE_SLOTS], live: 0 }
    }

    /// 显式注册进程频率配置（spawn 路径调用）。重复注册 = 重设配置并清零
    /// 计数与粘性标志（进程重启复用语义）。容量满返回 `NoSpace`。
    pub fn register(
        &mut self,
        pid: u32,
        window: u64,
        syscall_limit: u32,
        ipc_limit: u32,
    ) -> Result<(), SchedError> {
        if window == 0 {
            return Err(SchedError::InvalidArg);
        }
        if let Some(e) = self.find_mut(pid) {
            e.syscall = WindowCounter::new(window, syscall_limit);
            e.ipc = WindowCounter::new(window, ipc_limit);
            return Ok(());
        }
        if self.live >= MAX_RATE_SLOTS {
            return Err(SchedError::NoSpace);
        }
        let idx = self.entries.iter().position(|e| e.is_none()).ok_or(SchedError::NoSpace)?;
        self.entries[idx] = Some(RateEntry {
            pid,
            syscall: WindowCounter::new(window, syscall_limit),
            ipc: WindowCounter::new(window, ipc_limit),
        });
        self.live += 1;
        Ok(())
    }

    /// 移除进程频率条目（exit/reap 路径调用）。无条目返回 `NotFound`。
    pub fn unregister(&mut self, pid: u32) -> Result<(), SchedError> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.is_some_and(|e| e.pid == pid))
            .ok_or(SchedError::NotFound)?;
        self.entries[idx] = None;
        self.live -= 1;
        Ok(())
    }

    /// 记一次 syscall（内核 syscall 入口调用）。未注册 pid 自动以默认配置
    /// 建条目（消除 boot 顺序陷阱；槽满则 `NoSpace`）。返回是否超限。
    pub fn record_syscall(&mut self, pid: u32, now: u64) -> Result<bool, SchedError> {
        Ok(self.entry_auto(pid)?.syscall.record(now))
    }

    /// 记一次 IPC send（ipc 集成路径调用）。语义同 [`record_syscall`]。
    pub fn record_ipc(&mut self, pid: u32, now: u64) -> Result<bool, SchedError> {
        Ok(self.entry_auto(pid)?.ipc.record(now))
    }

    /// 查询 syscall 维粘性超限标志（无条目 = false）。
    pub fn is_over_syscall(&self, pid: u32) -> bool {
        self.find(pid).is_some_and(|e| e.syscall.over)
    }

    /// 查询 IPC 维粘性超限标志（无条目 = false）。
    pub fn is_over_ipc(&self, pid: u32) -> bool {
        self.find(pid).is_some_and(|e| e.ipc.over)
    }

    /// 任一维超限（监督树轮询入口）。
    pub fn is_over(&self, pid: u32) -> bool {
        self.is_over_syscall(pid) || self.is_over_ipc(pid)
    }

    /// 清除粘性标志（监督方处置完毕后调用；窗口计数不清零——
    /// 同窗口内再超限会立即重新置位）。
    pub fn clear_over(&mut self, pid: u32) {
        if let Some(e) = self.find_mut(pid) {
            e.syscall.over = false;
            e.ipc.over = false;
        }
    }

    /// 当前窗口计数观测（审计/调试用）。返回 (syscall, ipc)；无条目 (0, 0)。
    pub fn counts_of(&self, pid: u32) -> (u32, u32) {
        self.find(pid).map_or((0, 0), |e| (e.syscall.count, e.ipc.count))
    }

    /// 有条目的进程数。
    pub fn live_count(&self) -> usize {
        self.live
    }

    fn find(&self, pid: u32) -> Option<&RateEntry> {
        self.entries.iter().flatten().find(|e| e.pid == pid)
    }

    fn find_mut(&mut self, pid: u32) -> Option<&mut RateEntry> {
        self.entries.iter_mut().flatten().find(|e| e.pid == pid)
    }

    fn entry_auto(&mut self, pid: u32) -> Result<&mut RateEntry, SchedError> {
        if self.find_mut(pid).is_none() {
            self.register(pid, DEFAULT_RATE_WINDOW, DEFAULT_SYSCALL_LIMIT, DEFAULT_IPC_LIMIT)?;
        }
        Ok(self.find_mut(pid).expect("just registered"))
    }
}

impl Default for RateTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_limit_sets_sticky_flag() {
        let mut r = RateTable::new();
        r.register(1, 100, 3, 3).unwrap();
        for i in 0..3 {
            assert!(!r.record_syscall(1, i).unwrap()); // 未超限
        }
        assert!(r.record_syscall(1, 3).unwrap()); // 第 4 次超限
        assert!(r.is_over_syscall(1));
        assert!(r.is_over(1));
        // 粘性：窗口内即使不再记录也保持
        assert!(r.is_over_syscall(1));
    }

    #[test]
    fn window_rollover_resets_count_flag_sticky() {
        let mut r = RateTable::new();
        r.register(1, 10, 2, 2).unwrap();
        r.record_syscall(1, 0).unwrap();
        r.record_syscall(1, 1).unwrap();
        assert!(r.record_syscall(1, 2).unwrap()); // 超限
        // 新窗口（now >= start+10）：计数重置，不再超限；但旧粘性标志保持
        assert!(r.record_syscall(1, 10).unwrap()); // over 仍粘
        assert_eq!(r.counts_of(1).0, 1); // 计数已重置为本次 1
        // clear 后新窗口内未超限 → false
        r.clear_over(1);
        assert!(!r.record_syscall(1, 11).unwrap());
        assert!(!r.is_over(1));
    }

    #[test]
    fn clear_over_rearms_same_window() {
        let mut r = RateTable::new();
        r.register(1, 100, 1, 1).unwrap();
        r.record_syscall(1, 0).unwrap();
        assert!(r.record_syscall(1, 1).unwrap()); // 超限
        r.clear_over(1);
        assert!(!r.is_over(1));
        // 同窗口再记 → 计数 3 > limit 1 → 立即重新置位
        assert!(r.record_syscall(1, 2).unwrap());
        assert!(r.is_over(1));
    }

    #[test]
    fn syscall_and_ipc_independent() {
        let mut r = RateTable::new();
        r.register(1, 100, 1, 5).unwrap();
        r.record_syscall(1, 0).unwrap();
        assert!(r.record_syscall(1, 1).unwrap()); // syscall 超限
        assert!(!r.is_over_ipc(1)); // IPC 维不受影响
        assert!(!r.record_ipc(1, 2).unwrap());
        assert!(r.is_over(1)); // 任一维超限即 true
    }

    #[test]
    fn auto_create_on_unregistered_pid() {
        let mut r = RateTable::new();
        assert_eq!(r.live_count(), 0);
        assert!(!r.record_syscall(42, 0).unwrap());
        assert_eq!(r.live_count(), 1); // 自动建条目（默认配置）
        assert_eq!(r.counts_of(42), (1, 0));
        // 默认限额 = 10000，第 10001 次才超限（同一 tick 内，避开窗口滚动）
        for i in 1..=DEFAULT_SYSCALL_LIMIT {
            let over = r.record_syscall(42, 0).unwrap();
            assert_eq!(over, i == DEFAULT_SYSCALL_LIMIT); // 仅最后一次
        }
        assert!(r.is_over_syscall(42));
    }

    #[test]
    fn register_reregister_unregister() {
        let mut r = RateTable::new();
        r.register(7, 50, 5, 5).unwrap();
        for i in 0..6 {
            r.record_syscall(7, i).unwrap();
        }
        assert!(r.is_over(7));
        r.register(7, 50, 5, 5).unwrap(); // 重复注册 = 清零重置
        assert!(!r.is_over(7));
        assert_eq!(r.counts_of(7), (0, 0));
        r.unregister(7).unwrap();
        assert_eq!(r.unregister(7), Err(SchedError::NotFound));
        assert_eq!(r.live_count(), 0);
        assert!(!r.is_over(7)); // 无条目 = 不超限
    }

    #[test]
    fn invalid_args_and_capacity() {
        let mut r = RateTable::new();
        assert_eq!(r.register(1, 0, 5, 5), Err(SchedError::InvalidArg)); // 零窗口
        for pid in 0..MAX_RATE_SLOTS as u32 {
            r.register(pid, 100, 5, 5).unwrap();
        }
        assert_eq!(r.register(9999, 100, 5, 5), Err(SchedError::NoSpace));
        // 自动建条目路径同样受容量约束
        assert_eq!(r.record_ipc(8888, 0), Err(SchedError::NoSpace));
        r.unregister(3).unwrap(); // 腾位后可注册
        r.register(9999, 100, 5, 5).unwrap();
    }

    #[test]
    fn time_regression_does_not_roll_window() {
        let mut r = RateTable::new();
        r.register(1, 10, 2, 2).unwrap();
        r.record_syscall(1, 100).unwrap(); // window_start=100? 首记 now>=0+10 → 滚动
        assert_eq!(r.counts_of(1).0, 1);
        // now 回退到 95：不滚动（防御），继续累加
        r.record_syscall(1, 95).unwrap();
        assert_eq!(r.counts_of(1).0, 2);
        assert!(r.record_syscall(1, 96).unwrap()); // 3 > 2 超限
    }
}
