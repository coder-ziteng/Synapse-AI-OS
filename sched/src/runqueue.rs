//! `RunQueue`：NQ 级优先级位图 + 每级 FIFO 环，实现"跨级严格优先、同级 Round-Robin"。
//!
//! 纯逻辑：只存 [`ThreadId`]，不触碰 TCB（状态迁移由 [`crate::tcb::ThreadTable`] 校验）。
//! 固定容量：每级环最多 [`crate::MAX_THREADS`] 项（总容量 NQ × MAX_THREADS，
//! 实际不可能超过 MAX_THREADS 个线程同时在队——每线程至多入队一次，由
//! Scheduler 的状态机保证）。

use crate::tcb::MAX_THREADS;
use crate::types::{SchedError, ThreadId, NQ};

/// 单级 FIFO 环。
#[derive(Clone, Copy, Debug)]
struct Ring {
    slots: [ThreadId; MAX_THREADS],
    head: usize,
    len: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring { slots: [ThreadId(0); MAX_THREADS], head: 0, len: 0 }
    }

    fn push(&mut self, id: ThreadId) -> Result<(), SchedError> {
        if self.len == MAX_THREADS {
            return Err(SchedError::NoSpace);
        }
        let tail = (self.head + self.len) % MAX_THREADS;
        self.slots[tail] = id;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<ThreadId> {
        if self.len == 0 {
            return None;
        }
        let id = self.slots[self.head];
        self.head = (self.head + 1) % MAX_THREADS;
        self.len -= 1;
        Some(id)
    }

    /// 从队中移除指定线程（block/freeze/exit 路径）；返回是否找到。
    fn remove(&mut self, id: ThreadId) -> bool {
        let pos = (0..self.len).find(|&k| self.slots[(self.head + k) % MAX_THREADS] == id);
        let Some(pos) = pos else { return false };
        // 将 pos 之后的元素前移一格（保持 FIFO 相对序）。
        for k in pos..self.len - 1 {
            let cur = (self.head + k) % MAX_THREADS;
            let nxt = (self.head + k + 1) % MAX_THREADS;
            self.slots[cur] = self.slots[nxt];
        }
        self.len -= 1;
        true
    }
}

/// NQ 级优先级就绪队列。位图 bit q = 第 q 级非空（单 u8 即可，NQ=8）。
#[derive(Clone, Debug)]
pub struct RunQueue {
    rings: [Ring; NQ],
    bitmap: u8,
    len: usize,
}

impl RunQueue {
    /// 创建空队列。
    pub fn new() -> Self {
        RunQueue { rings: [Ring::new(); NQ], bitmap: 0, len: 0 }
    }

    /// 入队（priority 越小数越高，0 最高）。
    pub fn push(&mut self, prio: u8, id: ThreadId) -> Result<(), SchedError> {
        if prio as usize >= NQ {
            return Err(SchedError::InvalidArg);
        }
        self.rings[prio as usize].push(id)?;
        self.bitmap |= 1 << prio;
        self.len += 1;
        Ok(())
    }

    /// 取最高优先级队头（跨级严格优先；同级 FIFO = RR）。空返回 None。
    pub fn pop(&mut self) -> Option<ThreadId> {
        if self.bitmap == 0 {
            return None;
        }
        // trailing_zeros = 最低置位 bit = 数值最小的优先级 = 最高优先。
        let q = self.bitmap.trailing_zeros() as usize;
        let id = self.rings[q].pop();
        if self.rings[q].len == 0 {
            self.bitmap &= !(1 << q);
        }
        if id.is_some() {
            self.len -= 1;
        }
        id
    }

    /// 查看最高优先级队头但不移除。
    pub fn peek(&self) -> Option<ThreadId> {
        if self.bitmap == 0 {
            return None;
        }
        let q = self.bitmap.trailing_zeros() as usize;
        self.rings[q].slots.get(self.rings[q].head).copied()
    }

    /// 从任意优先级移除指定线程（调用方保证该线程确实处于 Ready 态）。
    /// 返回是否找到并移除。
    pub fn remove(&mut self, id: ThreadId) -> bool {
        for (q, ring) in self.rings.iter_mut().enumerate() {
            if ring.remove(id) {
                if ring.len == 0 {
                    self.bitmap &= !(1 << q);
                }
                self.len -= 1;
                return true;
            }
        }
        false
    }

    /// 队内线程总数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 队列是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 最高就绪优先级（空返回 None）；Scheduler 用于抢占判定。
    pub fn best_priority(&self) -> Option<u8> {
        if self.bitmap == 0 {
            None
        } else {
            Some(self.bitmap.trailing_zeros() as u8)
        }
    }
}

impl Default for RunQueue {
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
    fn fifo_within_same_priority() {
        let mut rq = RunQueue::new();
        for i in 0..4 {
            rq.push(2, id(i)).unwrap();
        }
        for i in 0..4 {
            assert_eq!(rq.pop(), Some(id(i))); // FIFO = RR 轮转序
        }
        assert!(rq.is_empty());
    }

    #[test]
    fn strict_priority_across_levels() {
        let mut rq = RunQueue::new();
        rq.push(3, id(1)).unwrap();
        rq.push(0, id(2)).unwrap();
        rq.push(3, id(3)).unwrap();
        rq.push(1, id(4)).unwrap();
        assert_eq!(rq.best_priority(), Some(0));
        assert_eq!(rq.pop(), Some(id(2))); // prio 0 最先
        assert_eq!(rq.pop(), Some(id(4))); // prio 1
        assert_eq!(rq.pop(), Some(id(1))); // prio 3 FIFO
        assert_eq!(rq.pop(), Some(id(3)));
        assert_eq!(rq.pop(), None);
    }

    #[test]
    fn bitmap_tracks_empty_levels() {
        let mut rq = RunQueue::new();
        rq.push(5, id(1)).unwrap();
        assert_eq!(rq.bitmap, 1 << 5);
        rq.pop();
        assert_eq!(rq.bitmap, 0); // 空级清位
        assert_eq!(rq.peek(), None);
    }

    #[test]
    fn remove_preserves_fifo_order() {
        let mut rq = RunQueue::new();
        for i in 0..5 {
            rq.push(1, id(i)).unwrap();
        }
        assert!(rq.remove(id(2))); // 摘中间
        assert!(!rq.remove(id(9))); // 不在队 → false
        assert_eq!(rq.len(), 4);
        for i in [0, 1, 3, 4] {
            assert_eq!(rq.pop(), Some(id(i)));
        }
    }

    #[test]
    fn ring_wraparound() {
        let mut rq = RunQueue::new();
        // push/pop 交替超过 MAX_THREADS 次强制 head 回绕
        for round in 0..(MAX_THREADS + 10) {
            rq.push(0, id(round as u16)).unwrap();
            assert_eq!(rq.pop(), Some(id(round as u16)));
        }
        assert!(rq.is_empty());
    }

    #[test]
    fn invalid_priority_rejected() {
        let mut rq = RunQueue::new();
        assert_eq!(rq.push(NQ as u8, id(1)), Err(SchedError::InvalidArg));
    }

    #[test]
    fn peek_does_not_consume() {
        let mut rq = RunQueue::new();
        rq.push(2, id(7)).unwrap();
        assert_eq!(rq.peek(), Some(id(7)));
        assert_eq!(rq.peek(), Some(id(7)));
        assert_eq!(rq.len(), 1);
    }
}
