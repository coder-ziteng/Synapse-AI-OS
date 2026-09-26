//! IRQ-safe 自旋锁（内核集成层锁语义）。
//!
//! cap/ipc/proc 三个纯逻辑 crate 均声明"无锁，锁语义由内核集成层负责"
//! （Spinlock + 关中断临界区）。本模块提供该包装：
//!
//! * `lock()` 保存 RFLAGS.IF → `cli` → 自旋获取 → 返回 guard；
//! * guard `Drop` 释放锁并按保存的 IF 位决定是否恢复中断
//!   （嵌套/中断上下文获取锁不会意外开中断）。
//!
//! MVP-3 单线程（Doc 02 §5.1）：当前无 SMP、无 IDT，关中断与自旋
//! 主要是语义占位——为 Phase 2 (IDT/PIC) 与后续多核做好准备，
//! 保证临界区约定从第一天就正确。

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::interrupts;
use x86_64::registers::rflags::{self, RFlags};

/// IRQ-safe 自旋锁。
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: T: Send 时跨核共享安全——临界区内独占访问，
// 且获取锁时已关中断（本核不会被抢占）。
unsafe impl<T: Send> Sync for SpinLock<T> {}

/// [`SpinLock::lock`] 返回的 RAII guard；Drop 时释放锁并恢复中断状态。
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// 进入临界区前 IF 是否为 1（是 → Drop 时重新开中断）。
    restore_if: bool,
}

impl<T> SpinLock<T> {
    /// 常量构造（供 `static` 初始化）。
    pub const fn new(value: T) -> SpinLock<T> {
        SpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// 获取锁：保存 IF → 关中断 → 自旋。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let flags = rflags::read();
        interrupts::disable();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // 单核 MVP：竞争只可能来自（未来的）中断上下文——
            // 已关中断，此处自旋仅为 SMP 预留。
            core::hint::spin_loop();
        }
        SpinLockGuard {
            lock: self,
            restore_if: flags.contains(RFlags::INTERRUPT_FLAG),
        }
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: guard 存活期间独占访问（锁已持有 + 本核中断已关）。
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 同上，&mut 独占由锁保证。
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.restore_if {
            interrupts::enable();
        }
    }
}
