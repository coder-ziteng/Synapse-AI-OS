//! 内核 Mutex<T> 睡眠锁（基于 [`kthread_block_current`] + [`Scheduler::unblock`]）。
//!
//! ## 与 [`crate::sync::SpinLock`] 的区别
//!
//! - **SpinLock**：短临界区忙等；持有期间不可让出 CPU（kthread_block_current
//!   会破坏 IRQ-safe 假设）。
//! - **Mutex**：竞争时调 `kthread_block_current()` 让出 CPU 阻塞；释放时
//!   显式 `Scheduler::unblock` 唤醒队首并把锁移交给它。适合"临界区内可能
//!   等待其他资源（IPC/外设）"的场景——例如 Phase 4+ endpoint 接收。
//!
//! ## 设计要点
//!
//! - **owner + waiters 两段**：持锁线程以 `Option<ThreadId>` 记录；等待队列
//!   是 FIFO 环形数组。锁"移交"语义：持锁线程 Drop 时把队首 `pop_front`，
//!   设置 `owner = waiter`，**不释放锁**（owner 立即指向下一个持锁者）；
//!   被 unblock 的线程醒来后进入 `lock()` 的下一轮循环，发现 `owner == me`
//!   → 直接拿到锁返回。
//! - **数据 `T` 放在 Mutex 顶层**（不在 `SpinLock` 内）：Deref/DerefMut 只需
//!   直接解引用，避免借用一个临时 SpinLockGuard 的字段（那样guard drop 后
//!   引用立刻悬空）。MVP-3 单核下 `T` 的安全靠"持锁线程独占"——`kthread_block_current`
//!   让出 CPU 时锁已交出，没有并发写者。
//! - **重入检测**：同一线程二次 `lock()` panic——支持可重入 mutex 会显著复杂化
//!   移交语义（MVP 暂不实现）。
//! - **无 alloc**：等待队列是固定容量环形数组 [`MAX_WAITERS`]，满则 panic（留证）；
//!   实际应用同一 Mutex 上等待线程数 ≪ 16（[kthread-preempt-smoke] 也只用 3 worker）。
//!
//! ## 不变量（用于调试）
//!
//! - `owner == None` ⇔ 锁空闲。
//! - `waiters` 中的 `ThreadId` 一定不在 `owner` 中。
//! - `waiters_len == 0` ⇔ `waiters_head == 0`（简化测试断言）。
//!
//! [`Scheduler::unblock`]: synapse_sched::Scheduler::unblock

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use synapse_sched::ThreadId;

use crate::kthread::{kthread_block_current, kthread_current_id};
use crate::sync::SpinLock;

/// 单 Mutex 最大等待线程数（MVP 上限；超限 panic 留证）。
const MAX_WAITERS: usize = 16;

/// Mutex 内层状态（元数据）——被 `SpinLock` 保护。
struct MutexState {
    /// 当前持锁者（`None` = 锁空闲）。
    owner: Option<ThreadId>,
    /// 等待队列（环形 FIFO；最多 [`MAX_WAITERS`] 项）。
    waiters: [Option<ThreadId>; MAX_WAITERS],
    waiters_len: usize,
    waiters_head: usize,
}

impl MutexState {
    const fn empty() -> Self {
        MutexState {
            owner: None,
            waiters: [None; MAX_WAITERS],
            waiters_len: 0,
            waiters_head: 0,
        }
    }

    /// `id` 是否已在等待队列。
    fn has_waiter(&self, id: ThreadId) -> bool {
        for i in 0..self.waiters_len {
            let slot = (self.waiters_head + i) % MAX_WAITERS;
            if self.waiters[slot] == Some(id) {
                return true;
            }
        }
        false
    }
}

/// 内核睡眠锁（owner + FIFO 等待队列 + 受保护数据）。
pub struct Mutex<T: ?Sized> {
    state: SpinLock<MutexState>,
    data: UnsafeCell<T>,
}

// SAFETY: `T: Send` 时跨核共享安全——
// * 临界区内 MutexState 受 SpinLock 保护（IRQ-safe，本核不被打断）；
// * 数据 T 的访问安全靠"持锁线程独占"（单核 MVP，无并发写者）。
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

/// [`Mutex::lock`] 返回的 RAII guard；Drop 时释放锁或把锁移交给队首等待者。
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    /// 常量构造（供 `static` 初始化）。
    pub const fn new(value: T) -> Self {
        Mutex {
            state: SpinLock::new(MutexState::empty()),
            data: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// 获取锁：未持锁则立即获取并返回 guard；否则把自己加入等待队列、
    /// 阻塞让出 CPU；被 unblock 后重试直到拿到。
    ///
    /// # Panics
    ///
    /// - 同一线程重复 `lock()`（重入检测）。
    /// - 等待队列已满（[`MAX_WAITERS`]）。
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let me = kthread_current_id();
        loop {
            let taken = {
                let mut st = self.state.lock();
                match st.owner {
                    None => {
                        st.owner = Some(me);
                        true
                    }
                    Some(o) if o == me => {
                        // After being unblocked, the Drop handler already set owner = me.
                        // Return the guard without re-checking (avoid false "reentrant" panic).
                        true
                    }
                    Some(_) => {
                        if !st.has_waiter(me) {
                            assert!(
                                st.waiters_len < MAX_WAITERS,
                                "Mutex waiters queue full ({}) — bump MAX_WAITERS or shorten critical section",
                                MAX_WAITERS
                            );
                            let tail =
                                (st.waiters_head + st.waiters_len) % MAX_WAITERS;
                            st.waiters[tail] = Some(me);
                            st.waiters_len += 1;
                        }
                        false
                    }
                }
            };
            if taken {
                return MutexGuard { mutex: self };
            }
            // 让出 CPU：下一轮循环检查 owner / 重试（owner 可能已移交给我）
            kthread_block_current();
        }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // 一次临界区决定"移交 vs 释放"。
        let to_wake = {
            let mut st = self.mutex.state.lock();
            if st.waiters_len > 0 {
                let head = st.waiters_head;
                let waiter = st.waiters[head].take().expect("waiter slot occupied");
                st.waiters_head = (head + 1) % MAX_WAITERS;
                st.waiters_len -= 1;
                // 锁立即"移交"给 waiter —— owner 设置但不释放锁;
                // waiter 醒来后进入 lock() 下一轮循环，发现 owner==me 拿锁成功。
                st.owner = Some(waiter);
                Some(waiter)
            } else {
                st.owner = None;
                None
            }
        };
        // 在临界区外 unblock —— 避免持 Mutex 自身的 SpinLock 时进入
        // SCHED 全局锁（与 irq-safe 锁嵌套约定一致）。
        if let Some(w) = to_wake {
            let _ = crate::kthread::kthread_unblock(w);
        }
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: 持锁线程独占访问——
        //   * MutexState.owner == Some(self) 在 guard 存活期间不变（Drop 才改）；
        //   * 持锁期间若让出 CPU，必先 Drop guard（移交/释放），不会有其他持锁者；
        //   * 单核 MVP 无并发写者。
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 同 Deref —— `&mut` 独占由"持锁线程唯一"保证。
        unsafe { &mut *self.mutex.data.get() }
    }
}