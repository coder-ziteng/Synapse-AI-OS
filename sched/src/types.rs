//! 基础类型：`ThreadId` / `ThreadState` / `Priority` / `ContextSlot` / `SchedError`。

/// 调度优先级级数（0 = 最高优先级，NQ-1 = 最低）。
///
/// MVP 取 8 级：够 Agent 监督树表达"看门狗 > 外交工具 > 普通 Agent > 批处理"
/// 的典型分层，又保持 RunQueue 位图一个 u8 装下。
pub const NQ: usize = 8;

/// 线程 ID：低 16 位 = 槽位索引，高 16 位 = generation。
///
/// generation 复用防 ABA（与 cap::ObjRef 同一纪律）：槽位释放再分配后 gen+1，
/// 持有旧 ThreadId 的一方查找会得到 `NotFound`，不会误操作新线程。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ThreadId(pub u32);

impl ThreadId {
    /// 由槽位索引与 generation 构造。
    pub const fn new(index: u16, gen: u16) -> Self {
        ThreadId(((gen as u32) << 16) | (index as u32))
    }

    /// 槽位索引（< [`crate::MAX_THREADS`]）。
    pub const fn index(self) -> usize {
        (self.0 & 0xFFFF) as usize
    }

    /// generation 计数。
    pub const fn gen(self) -> u16 {
        (self.0 >> 16) as u16
    }
}

/// 线程状态机。
///
/// 合法迁移（由 [`crate::tcb::ThreadTable::transition`] 强制校验）：
///
/// ```text
///            spawn
///              │
///              v
///   ┌──────> Ready <────────┐
///   │        │  ^           │ wake_due(deadline 到期)
///   │ pick   │  │ preempt/  │
///   │        v  │ yield     │
///   │      Running ─────────┤
///   │        │  │  │        │
///   │ block  │  │  │ sleep  │
///   │        v  │  v        │
///   │     Blocked│ Sleeping ┘
///   │        │   │
///   │ unblock│  │
///   └────────┘  └──(exit)──> Exited ──(reap)──> [槽位释放]
///
///   freeze: 任意非 Exited 状态 → Frozen（从就绪/睡眠队列摘除）
///   thaw:   Frozen → Ready（重新入就绪队列）
/// ```
///
/// Frozen 是 FR10（P3-T3）监督树原语：冻结线程不参与调度，
/// 但保留 TCB 与队列外状态，解冻后回到 Ready。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    /// 在就绪队列中，等待被 pick。
    Ready,
    /// 当前正在 CPU 上运行（单核 MVP：同一时刻至多一个）。
    Running,
    /// 睡眠中（带 deadline），到期由 `wake_due` 迁回 Ready。
    Sleeping,
    /// 阻塞点等待（锁/IPC），由 `unblock` 显式迁回 Ready。
    Blocked,
    /// 已退出（终态前站）：等待 reap 释放槽位。任何调度操作返回 BadState。
    Exited,
    /// 已冻结（FR10）：不参与调度，等待 thaw。
    Frozen,
}

/// 调度优先级：0 = 最高，[`NQ`]-1 = 最低（MVP 静态优先级，无动态提升）。
///
/// 语义约定：**数值越小越优先**（与 Unix nice 值同向），
/// `a.0 < b.0` 即 a 抢占 b。RunQueue 位图按此序取 `trailing_zeros`。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Priority(pub u8);

impl Priority {
    /// 最高优先级（看门狗/关键内核线程）。
    pub const HIGH: Priority = Priority(0);
    /// 默认优先级（普通内核线程/Agent）。
    pub const DEFAULT: Priority = Priority(3);
    /// 最低优先级（批处理/idle 类）。
    pub const LOW: Priority = Priority((NQ - 1) as u8);

    /// 是否在合法区间（< NQ）。
    pub const fn valid(self) -> bool {
        (self.0 as usize) < NQ
    }
}

/// 调度错误码。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    /// 线程槽位耗尽（[`crate::MAX_THREADS`] 上限）。
    NoSpace,
    /// ThreadId 无效：索引越界或 generation 不匹配（槽位已复用）。
    NotFound,
    /// 非法状态迁移（如对 Exited 线程 unblock）。
    BadState,
    /// 参数非法（如优先级 ≥ NQ、deadline 溢出）。
    InvalidArg,
}

/// CPU 上下文槽位（对本 crate 不透明）。
///
/// 16 个 u64 字：覆盖 x86_64 SysV ABI 全部 callee-saved 寄存器
/// （rbx/rbp/r12~r15 = 6 字）+ rsp + rip = 8 字，余 8 字预留
/// （P3-T5 的 switch_to trampoline 蹦床参数 / 未来 FPU 惰性保存标志）。
///
/// 纯逻辑 crate 不解释内容；内核集成层（P3-T4/T5）按约定读写。
/// 全零 = 新线程尚未首次切换（switch_to 需走蹦床入口而非恢复路径）。
///
/// `repr(C)`：内核 `switch_to` 汇编按固定字节偏移（0x00..0x38）直接读写
/// `words[0..8]`，且以裸指针跨 FFI 边界传递——必须保证 `words` 位于偏移 0
/// 且布局稳定（下方 tests 有编译期断言）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ContextSlot {
    /// 上下文原始字存储。
    pub words: [u64; CTX_WORDS],
}

/// [`ContextSlot`] 的字数。
pub const CTX_WORDS: usize = 16;

impl ContextSlot {
    /// 全零上下文（新线程初始状态）。
    pub const fn zeroed() -> Self {
        ContextSlot { words: [0; CTX_WORDS] }
    }

    /// 是否为全零（= 从未切换过的新线程，switch_to 走蹦床入口）。
    pub fn is_fresh(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }
}

impl Default for ContextSlot {
    fn default() -> Self {
        Self::zeroed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_packs_index_and_gen() {
        let id = ThreadId::new(0x1234, 0xABCD);
        assert_eq!(id.0, 0xABCD_1234);
        assert_eq!(id.index(), 0x1234);
        assert_eq!(id.gen(), 0xABCD);
    }

    #[test]
    fn thread_id_gen_reuse_changes_value() {
        let a = ThreadId::new(3, 1);
        let b = ThreadId::new(3, 2);
        assert_ne!(a, b); // 同槽位不同代 → 不等，旧持有者查找将 NotFound
    }

    #[test]
    fn context_slot_fresh_detection() {
        let mut ctx = ContextSlot::zeroed();
        assert!(ctx.is_fresh());
        ctx.words[0] = 0x2000; // 模拟内核写入 rsp
        assert!(!ctx.is_fresh());
    }

    #[test]
    fn context_slot_size_fits_callee_saved() {
        // rbx rbp r12 r13 r14 r15 rsp rip = 8 ≤ CTX_WORDS（编译期断言）
        const _: () = assert!(CTX_WORDS >= 8);
        // repr(C) + words 在偏移 0：内核 switch_to 汇编按 0x00..0x38 固定偏移访问
        assert_eq!(core::mem::offset_of!(ContextSlot, words), 0);
        assert_eq!(core::mem::size_of::<ContextSlot>(), CTX_WORDS * 8);
    }
}
