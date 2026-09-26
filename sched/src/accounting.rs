//! FR8 资源核算原语：per-process 内存页计数器（`PageLedger`）。
//!
//! CPU 时间记账在 [`crate::scheduler::Scheduler::account_cpu`]（per-thread
//! `Tcb::cpu_time` 饱和累加）；本模块补齐内存维度：内核页帧分配器每次
//! alloc/free 页时经此记账，进程退出时 `drain` 返回泄漏页数供审计（FR9）。
//!
//! 纯逻辑：只做 u64 计数，不触碰真实内存；槽位按 pid 线性占用
//! （与 proc crate 的 `MAX_PROCS=128` 对齐），pid=0 保留给内核自身。

use crate::types::SchedError;

/// 核算槽位数（= proc crate MAX_PROCS：每进程至多一条账目）。
pub const MAX_PROC_LEDGERS: usize = 128;

/// 单进程账目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LedgerEntry {
    pid: u32,
    pages: u64,
}

/// 固定容量进程页账本。
#[derive(Clone, Debug)]
pub struct PageLedger {
    entries: [Option<LedgerEntry>; MAX_PROC_LEDGERS],
    live: usize,
    /// 全部进程在册页数之和（守恒校验用）。
    grand_total: u64,
}

impl PageLedger {
    /// 创建空账本。
    pub fn new() -> Self {
        PageLedger { entries: [None; MAX_PROC_LEDGERS], live: 0, grand_total: 0 }
    }

    /// 记账：进程 `pid` 新分配 `pages` 页（首次调用自动建条目）。
    pub fn alloc(&mut self, pid: u32, pages: u64) -> Result<(), SchedError> {
        if pages == 0 {
            return Err(SchedError::InvalidArg);
        }
        let e = self.entry_or_create(pid)?;
        e.pages = e.pages.saturating_add(pages);
        self.grand_total = self.grand_total.saturating_add(pages);
        Ok(())
    }

    /// 记账：进程 `pid` 释放 `pages` 页。超额释放返回 `InvalidArg` 且不改动
    /// （防记账漂移——超额即分配器/调用方 bug，必须显式暴露）。
    pub fn free(&mut self, pid: u32, pages: u64) -> Result<(), SchedError> {
        if pages == 0 {
            return Err(SchedError::InvalidArg);
        }
        let e = self.find_mut(pid).ok_or(SchedError::NotFound)?;
        if e.pages < pages {
            return Err(SchedError::InvalidArg);
        }
        e.pages -= pages;
        self.grand_total -= pages;
        Ok(())
    }

    /// 查询进程在册页数；无条目返回 0（未分配过 = 零占用）。
    pub fn pages_of(&self, pid: u32) -> u64 {
        self.find(pid).map_or(0, |e| e.pages)
    }

    /// 全部进程在册页数总和。
    pub fn total(&self) -> u64 {
        self.grand_total
    }

    /// 有账目的进程数。
    pub fn live_count(&self) -> usize {
        self.live
    }

    /// 进程退出清算：返回泄漏页数（应为其全部在册页），并移除条目。
    /// 泄漏数 > 0 由调用方（内核集成层）写入审计事件流（FR9）。
    /// 无条目返回 `NotFound`。
    pub fn drain(&mut self, pid: u32) -> Result<u64, SchedError> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.map_or(false, |e| e.pid == pid))
            .ok_or(SchedError::NotFound)?;
        let leaked = self.entries[idx].map(|e| e.pages).unwrap_or(0);
        self.entries[idx] = None;
        self.live -= 1;
        self.grand_total -= leaked;
        Ok(leaked)
    }

    fn find(&self, pid: u32) -> Option<&LedgerEntry> {
        self.entries.iter().flatten().find(|e| e.pid == pid)
    }

    fn find_mut(&mut self, pid: u32) -> Option<&mut LedgerEntry> {
        self.entries.iter_mut().flatten().find(|e| e.pid == pid)
    }

    fn entry_or_create(&mut self, pid: u32) -> Result<&mut LedgerEntry, SchedError> {
        if let Some(e) = self.find_mut(pid) {
            return Ok(e);
        }
        if self.live >= MAX_PROC_LEDGERS {
            return Err(SchedError::NoSpace);
        }
        let idx = self.entries.iter().position(|e| e.is_none()).ok_or(SchedError::NoSpace)?;
        self.entries[idx] = Some(LedgerEntry { pid, pages: 0 });
        self.live += 1;
        Ok(self.entries[idx].as_mut().expect("just set"))
    }
}

impl Default for PageLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_roundtrip() {
        let mut l = PageLedger::new();
        l.alloc(1, 10).unwrap();
        l.alloc(1, 5).unwrap();
        assert_eq!(l.pages_of(1), 15);
        assert_eq!(l.total(), 15);
        l.free(1, 12).unwrap();
        assert_eq!(l.pages_of(1), 3);
        assert_eq!(l.total(), 3);
    }

    #[test]
    fn zero_and_overfree_rejected() {
        let mut l = PageLedger::new();
        assert_eq!(l.alloc(1, 0), Err(SchedError::InvalidArg));
        assert_eq!(l.free(1, 1), Err(SchedError::NotFound)); // 无条目
        l.alloc(1, 4).unwrap();
        assert_eq!(l.free(1, 5), Err(SchedError::InvalidArg)); // 超额
        assert_eq!(l.pages_of(1), 4); // 失败不改动
        assert_eq!(l.total(), 4);
        assert_eq!(l.free(1, 0), Err(SchedError::InvalidArg));
    }

    #[test]
    fn multi_process_conservation() {
        let mut l = PageLedger::new();
        for pid in 1..=5u32 {
            l.alloc(pid, pid as u64 * 3).unwrap();
        }
        // 总和 = 3*(1+2+3+4+5) = 45
        assert_eq!(l.total(), 45);
        let sum: u64 = (1..=5u32).map(|p| l.pages_of(p)).sum();
        assert_eq!(sum, l.total()); // 守恒
        l.free(3, 9).unwrap();
        assert_eq!(l.total(), 36);
        assert_eq!(l.live_count(), 5); // 条目仍在（页数 0 ≠ 移除）
    }

    #[test]
    fn drain_reports_leak_and_removes() {
        let mut l = PageLedger::new();
        l.alloc(7, 8).unwrap();
        l.alloc(9, 2).unwrap();
        assert_eq!(l.drain(7), Ok(8)); // 泄漏 8 页（未 free 即退出）
        assert_eq!(l.pages_of(7), 0);
        assert_eq!(l.total(), 2); // 只剩 pid=9
        assert_eq!(l.live_count(), 1);
        assert_eq!(l.drain(7), Err(SchedError::NotFound)); // 二次 drain
        // 干净退出：free 到 0 再 drain → 泄漏 0
        l.free(9, 2).unwrap();
        assert_eq!(l.drain(9), Ok(0));
        assert_eq!(l.total(), 0);
        assert_eq!(l.live_count(), 0);
    }

    #[test]
    fn capacity_limit() {
        let mut l = PageLedger::new();
        for pid in 0..MAX_PROC_LEDGERS as u32 {
            l.alloc(pid, 1).unwrap();
        }
        assert_eq!(l.alloc(9999, 1), Err(SchedError::NoSpace));
        assert_eq!(l.live_count(), MAX_PROC_LEDGERS);
        // drain 腾出槽位后可再建
        l.drain(0).unwrap();
        l.alloc(9999, 1).unwrap();
        assert_eq!(l.live_count(), MAX_PROC_LEDGERS);
    }

    #[test]
    fn saturating_alloc_no_overflow() {
        let mut l = PageLedger::new();
        l.alloc(1, u64::MAX).unwrap();
        l.alloc(1, 1).unwrap(); // 饱和不 panic（no_std 下溢出 = 内核事故）
        assert_eq!(l.pages_of(1), u64::MAX);
        // free 到 0 保持 grand_total 一致
        l.free(1, u64::MAX).unwrap();
        assert_eq!(l.total(), 0);
    }
}
