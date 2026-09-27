//! 异步 Notification 对象（对齐 [Doc 03 §6.2](../../../docs/design/03-ipc-message-and-single-copy-path.md)）。
//!
//! seL4 式**位图语义**：`signal` 按位 OR（O(1)），多个 IRQ 源天然聚合到
//! 同一对象；`poll` 非零则取出并清零（读清语义）。不复用同步 Endpoint
//! （决策理由见 Doc 03 §6.2：语义不匹配 + 消费竞争 + 中断路径阻塞风险）。
//!
//! TBD（Doc 03 §9，待驱动模型确认）：mask/unmask 选择性忽略、跨进程共享。

/// 异步通知对象（位图 word，u64 支持最多 64 个聚合源）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    word: u64,
}

impl Notification {
    /// 创建空通知对象（无挂起事件）。
    pub const fn new() -> Notification {
        Notification { word: 0 }
    }

    /// 投递事件位（OR 语义，O(1)）。中断路径调用：
    /// 内核 IDT → signal(1 << irq_bit) → EOI → 用户态驱动 poll。
    ///
    /// `bits = 0` 为 no-op。
    pub fn signal(&mut self, bits: u64) {
        self.word |= bits;
    }

    /// 非阻塞取出：有挂起事件 → `Some(位图)` 并**读清**；无 → `None`。
    ///
    /// 返回 `None` 时内核集成层将调用线程置 Blocked（阻塞 wait 语义）；
    /// `try_wait` 变体则映射为 [`synapse_cap::CapError::WouldBlock`]。
    pub fn poll(&mut self) -> Option<u64> {
        if self.word == 0 {
            None
        } else {
            Some(core::mem::take(&mut self.word))
        }
    }

    /// 窥视当前挂起位图（不读清；调试 / 审计路径）。
    pub const fn peek(&self) -> u64 {
        self.word
    }

    /// 清除 `bits` 中已置位的位（位图 AND NOT 语义；P4-T8 wait mask 路径）。
    /// 返回清除前被清掉的位（即"曾经匹配"集合，给上层读清语义用）。
    pub fn clear_matched(&mut self, bits: u64) -> u64 {
        let prev = self.word & bits;
        self.word &= !bits;
        prev
    }
}
