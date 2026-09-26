//! `SleepQueue`：带 deadline 的睡眠队列。
//!
//! MVP 实现：无序数组 + `wake_due` 线性扫描（容量 ≤ [`crate::MAX_THREADS`]，
//! 单核 tick 频率 100Hz 下 O(n) 扫描每 tick 最多 64 次比较，成本可忽略）。
//! 若 Phase 4+ 睡眠线程数显著增长，可换二叉堆——接口已按"只经 deadline
//! 唤醒"设计，替换实现不影响 Scheduler。

use crate::tcb::MAX_THREADS;
use crate::types::{SchedError, ThreadId};

/// 睡眠项：线程 + 到期时刻（tick 时间基，由内核集成层注入）。
#[derive(Clone, Copy, Debug)]
struct Entry {
    id: ThreadId,
    deadline: u64,
}

/// 固定容量睡眠队列。
#[derive(Clone, Debug)]
pub struct SleepQueue {
    entries: [Option<Entry>; MAX_THREADS],
    len: usize,
}

impl SleepQueue {
    /// 创建空队列。
    pub fn new() -> Self {
        SleepQueue { entries: [None; MAX_THREADS], len: 0 }
    }

    /// 入队睡眠（deadline 为绝对 tick 时刻）。重复入队同一线程返回 InvalidArg。
    pub fn sleep(&mut self, id: ThreadId, deadline: u64) -> Result<(), SchedError> {
        if self.contains(id) {
            return Err(SchedError::InvalidArg);
        }
        let slot = self
            .entries
            .iter_mut()
            .find(|e| e.is_none())
            .ok_or(SchedError::NoSpace)?;
        *slot = Some(Entry { id, deadline });
        self.len += 1;
        Ok(())
    }

    /// 提前唤醒/取消（exit/freeze 路径摘除）；返回是否存在并被移除。
    pub fn cancel(&mut self, id: ThreadId) -> bool {
        let pos = self.entries.iter().position(|e| e.map(|e| e.id) == Some(id));
        let Some(pos) = pos else { return false };
        self.entries[pos] = None;
        self.len -= 1;
        true
    }

    /// 收集所有 deadline ≤ now 的线程（每 tick 调用一次），按到期先后排序
    /// 写入 `out`，返回唤醒个数。out 容量不足时截断（剩余下个 tick 再唤醒）。
    pub fn wake_due(&mut self, now: u64, out: &mut [ThreadId]) -> usize {
        let mut n = 0;
        while n < out.len() {
            // 找最早到期的项
            let mut best: Option<(usize, u64)> = None;
            for (i, e) in self.entries.iter().enumerate() {
                if let Some(e) = e {
                    if e.deadline <= now && best.map(|(_, d)| e.deadline < d).unwrap_or(true) {
                        best = Some((i, e.deadline));
                    }
                }
            }
            let Some((i, _)) = best else { break };
            out[n] = self.entries[i].map(|e| e.id).unwrap();
            self.entries[i] = None;
            self.len -= 1;
            n += 1;
        }
        n
    }

    /// 队列中是否有该线程。
    pub fn contains(&self, id: ThreadId) -> bool {
        self.entries.iter().any(|e| e.map(|e| e.id) == Some(id))
    }

    /// 当前睡眠线程数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 最早 deadline（空返回 None）；内核可用它在无就绪线程时决定 hlt 时长。
    pub fn next_deadline(&self) -> Option<u64> {
        self.entries.iter().flatten().map(|e| e.deadline).min()
    }
}

impl Default for SleepQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u16) -> ThreadId {
        ThreadId::new(i, 1)
    }

    #[test]
    fn wake_in_deadline_order() {
        let mut sq = SleepQueue::new();
        sq.sleep(id(1), 300).unwrap();
        sq.sleep(id(2), 100).unwrap();
        sq.sleep(id(3), 200).unwrap();
        let mut out = [ThreadId(0); 8];
        let n = sq.wake_due(250, &mut out); // 100/200 到期，300 未到
        assert_eq!(n, 2);
        assert_eq!(out[0], id(2));
        assert_eq!(out[1], id(3));
        assert_eq!(sq.len(), 1);
        assert!(sq.contains(id(1)));
    }

    #[test]
    fn boundary_deadline_eq_now_wakes() {
        let mut sq = SleepQueue::new();
        sq.sleep(id(1), 100).unwrap();
        let mut out = [ThreadId(0); 4];
        assert_eq!(sq.wake_due(99, &mut out), 0);
        assert_eq!(sq.wake_due(100, &mut out), 1); // deadline <= now 含等号
    }

    #[test]
    fn cancel_before_deadline() {
        let mut sq = SleepQueue::new();
        sq.sleep(id(1), 100).unwrap();
        assert!(sq.cancel(id(1)));
        assert!(!sq.cancel(id(1))); // 二次取消 → false
        let mut out = [ThreadId(0); 4];
        assert_eq!(sq.wake_due(200, &mut out), 0);
    }

    #[test]
    fn duplicate_sleep_rejected() {
        let mut sq = SleepQueue::new();
        sq.sleep(id(1), 100).unwrap();
        assert_eq!(sq.sleep(id(1), 200), Err(SchedError::InvalidArg));
    }

    #[test]
    fn out_buffer_truncates() {
        let mut sq = SleepQueue::new();
        for i in 0..5 {
            sq.sleep(id(i), 10).unwrap();
        }
        let mut out = [ThreadId(0); 2]; // 缓冲只装 2 个
        assert_eq!(sq.wake_due(100, &mut out), 2);
        assert_eq!(sq.len(), 3); // 剩余下个 tick 再唤醒
    }

    #[test]
    fn next_deadline_min() {
        let mut sq = SleepQueue::new();
        assert_eq!(sq.next_deadline(), None);
        sq.sleep(id(1), 300).unwrap();
        sq.sleep(id(2), 150).unwrap();
        assert_eq!(sq.next_deadline(), Some(150));
    }
}
