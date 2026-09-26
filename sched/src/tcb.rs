//! `ThreadTable`：固定槽位 TCB 存储 + generation 复用防 ABA + 状态迁移校验。
//!
//! 与 cap::ObjectTable 同一纪律：静态数组、O(1) 索引、槽位释放再分配时
//! generation +1，使旧 [`ThreadId`] 持有者的查找确定性失败（NotFound）。

use crate::types::{ContextSlot, Priority, SchedError, ThreadId, ThreadState};

/// 最大线程数（MVP 静态上限；内核集成层按 bss 预算调整）。
///
/// 64 = 单核 MVP 下"128 进程槽"的保守子集：Phase 3 只跑内核线程
/// （boot 线程 + smoke 线程 + 未来 idle 线程），64 绰绰有余。
pub const MAX_THREADS: usize = 64;

/// 线程控制块（纯数据，无硬件依赖）。
#[derive(Clone, Copy, Debug)]
pub struct Tcb {
    /// 线程 ID（含 generation）。
    pub id: ThreadId,
    /// 当前状态。
    pub state: ThreadState,
    /// 静态优先级（0 最高；MVP 不做动态提升）。
    pub priority: Priority,
    /// CPU 上下文槽位（对纯逻辑层不透明）。
    pub ctx: ContextSlot,
    /// 睡眠 deadline（仅 Sleeping 状态有意义；tick 时间基）。
    pub deadline: u64,
    /// 所属进程 PID 透传槽（FR8 per-process 记账用，P3-T2 消费；0 = 内核线程）。
    pub owner_pid: u32,
}

/// 槽位占用标记：空槽 gen 从 0 起，分配时 +1 → 有效 gen ≥ 1，
/// 全零 ThreadId(0)（index=0, gen=0）因此永远无效，可作 NULL 哨兵。
#[derive(Clone, Copy, Debug)]
struct Slot {
    used: bool,
    gen: u16,
    tcb: Tcb,
}

impl Slot {
    const EMPTY: Slot = Slot {
        used: false,
        gen: 0,
        tcb: Tcb {
            id: ThreadId(0),
            state: ThreadState::Exited,
            priority: Priority(0),
            ctx: ContextSlot::zeroed(),
            deadline: 0,
            owner_pid: 0,
        },
    };
}

/// 固定容量线程表。非 const 构造（与 cap/ipc/proc 一致，内核集成层用锁包装）。
#[derive(Clone, Debug)]
pub struct ThreadTable {
    slots: [Slot; MAX_THREADS],
    live: usize,
}

/// 校验状态迁移是否合法（types.rs 状态机图的唯一实现点）。
fn legal(from: ThreadState, to: ThreadState) -> bool {
    use ThreadState::*;
    match (from, to) {
        // spawn 入口由 alloc 直接置 Ready，不经 transition。
        (Ready, Running) | (Ready, Blocked) | (Ready, Sleeping) | (Ready, Exited) | (Ready, Frozen) => true,
        (Running, Ready) | (Running, Blocked) | (Running, Sleeping) | (Running, Exited) | (Running, Frozen) => true,
        (Sleeping, Ready) | (Sleeping, Exited) | (Sleeping, Frozen) => true,
        (Blocked, Ready) | (Blocked, Exited) | (Blocked, Frozen) => true,
        (Frozen, Ready) | (Frozen, Exited) => true,
        // Exited 是终态：只能被 reap（槽位释放），无状态迁移。
        _ => false,
    }
}

impl ThreadTable {
    /// 创建空表。
    pub fn new() -> Self {
        ThreadTable { slots: [Slot::EMPTY; MAX_THREADS], live: 0 }
    }

    /// 分配槽位并置为 Ready（spawn 的存储层半部；入队由 Scheduler 负责）。
    pub fn alloc(&mut self, priority: Priority, owner_pid: u32) -> Result<ThreadId, SchedError> {
        if self.live >= MAX_THREADS {
            return Err(SchedError::NoSpace);
        }
        let idx = self
            .slots
            .iter()
            .position(|s| !s.used)
            .ok_or(SchedError::NoSpace)?;
        let slot = &mut self.slots[idx];
        slot.gen = slot.gen.wrapping_add(1);
        slot.used = true;
        slot.tcb = Tcb {
            id: ThreadId::new(idx as u16, slot.gen),
            state: ThreadState::Ready,
            priority,
            ctx: ContextSlot::zeroed(),
            deadline: 0,
            owner_pid,
        };
        self.live += 1;
        Ok(slot.tcb.id)
    }

    /// 释放已 Exited 线程的槽位（reap）。gen 保留 → 下次 alloc 时 +1。
    pub fn release(&mut self, id: ThreadId) -> Result<(), SchedError> {
        let slot = self.slot_checked_mut(id)?;
        if slot.tcb.state != ThreadState::Exited {
            return Err(SchedError::BadState);
        }
        slot.used = false;
        self.live -= 1;
        Ok(())
    }

    /// 校验并执行状态迁移；非法迁移返回 BadState 且不改动。
    pub fn transition(&mut self, id: ThreadId, to: ThreadState) -> Result<(), SchedError> {
        let slot = self.slot_checked_mut(id)?;
        if !legal(slot.tcb.state, to) {
            return Err(SchedError::BadState);
        }
        slot.tcb.state = to;
        Ok(())
    }

    /// 只读访问 TCB。
    pub fn get(&self, id: ThreadId) -> Result<&Tcb, SchedError> {
        Ok(&self.slot_checked(id)?.tcb)
    }

    /// 可写访问 TCB（Scheduler 内部改 deadline / 内核层写 ctx）。
    pub fn get_mut(&mut self, id: ThreadId) -> Result<&mut Tcb, SchedError> {
        Ok(&mut self.slot_checked_mut(id)?.tcb)
    }

    /// 当前存活（已分配未 reap）线程数。
    pub fn live_count(&self) -> usize {
        self.live
    }

    /// generation + used 双重校验；失败返回 NotFound（防 ABA 的唯一入口）。
    fn slot_checked(&self, id: ThreadId) -> Result<&Slot, SchedError> {
        self.slots
            .get(id.index())
            .filter(|s| s.used && s.gen == id.gen())
            .ok_or(SchedError::NotFound)
    }

    /// 可变版槽位校验。
    fn slot_checked_mut(&mut self, id: ThreadId) -> Result<&mut Slot, SchedError> {
        let idx = id.index();
        let gen = id.gen();
        self.slots
            .get_mut(idx)
            .filter(|s| s.used && s.gen == gen)
            .ok_or(SchedError::NotFound)
    }
}

impl Default for ThreadTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_release_gen_bump() {
        let mut t = ThreadTable::new();
        let a = t.alloc(Priority(2), 0).unwrap();
        assert_eq!(a.index(), 0);
        assert_eq!(a.gen(), 1); // 首次分配 gen=1，ThreadId(0) 哨兵永久无效
        t.transition(a, ThreadState::Exited).unwrap();
        t.release(a).unwrap();
        let b = t.alloc(Priority(2), 0).unwrap();
        assert_eq!(b.index(), 0); // 同槽位复用
        assert_eq!(b.gen(), 2); // gen 已 +1
        // &Tcb 无 PartialEq：比较错误通道
        assert_eq!(t.get(a).err(), Some(SchedError::NotFound)); // 旧 ID 确定性失败
        assert!(t.get(b).is_ok());
    }

    #[test]
    fn release_requires_exited() {
        let mut t = ThreadTable::new();
        let a = t.alloc(Priority(0), 1).unwrap();
        assert_eq!(t.release(a), Err(SchedError::BadState)); // Ready 不可直接 reap
        t.transition(a, ThreadState::Exited).unwrap();
        t.release(a).unwrap();
        assert_eq!(t.live_count(), 0);
    }

    #[test]
    fn legal_transitions_matrix() {
        let mut t = ThreadTable::new();
        let a = t.alloc(Priority(1), 0).unwrap();
        // Ready → Running → Ready（抢占回队）
        t.transition(a, ThreadState::Running).unwrap();
        t.transition(a, ThreadState::Ready).unwrap();
        // Ready → Blocked → Ready（锁等待）
        t.transition(a, ThreadState::Blocked).unwrap();
        t.transition(a, ThreadState::Ready).unwrap();
        // Ready → Sleeping → Ready（定时睡眠）
        t.transition(a, ThreadState::Sleeping).unwrap();
        t.transition(a, ThreadState::Ready).unwrap();
        // freeze / thaw
        t.transition(a, ThreadState::Frozen).unwrap();
        t.transition(a, ThreadState::Ready).unwrap();
        // 终态
        t.transition(a, ThreadState::Exited).unwrap();
        assert_eq!(t.transition(a, ThreadState::Ready), Err(SchedError::BadState));
    }

    #[test]
    fn illegal_transitions_rejected() {
        let mut t = ThreadTable::new();
        let a = t.alloc(Priority(1), 0).unwrap();
        // Ready → Ready 非法（无意义迁移）
        assert_eq!(t.transition(a, ThreadState::Ready), Err(SchedError::BadState));
        // Blocked 直接 → Running 非法（必须经 Ready 被 pick）
        t.transition(a, ThreadState::Blocked).unwrap();
        assert_eq!(t.transition(a, ThreadState::Running), Err(SchedError::BadState));
    }

    #[test]
    fn capacity_exhaustion() {
        let mut t = ThreadTable::new();
        for _ in 0..MAX_THREADS {
            t.alloc(Priority(0), 0).unwrap();
        }
        assert_eq!(t.alloc(Priority(0), 0), Err(SchedError::NoSpace));
        assert_eq!(t.live_count(), MAX_THREADS);
    }

    #[test]
    fn stale_index_out_of_range() {
        let t = ThreadTable::new();
        let bogus = ThreadId::new(u16::MAX, 1);
        assert_eq!(t.get(bogus).err(), Some(SchedError::NotFound));
    }
}
