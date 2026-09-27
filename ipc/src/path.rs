//! 传输路径分类（对齐 [Doc 03 §4.3](../../../docs/design/03-ipc-message-and-single-copy-path.md)）。
//!
//! 三档路径（决策已在 Doc 03 §9 收敛）：
//!
//! | payload 大小 | 路径 | 说明 |
//! |--------------|------|------|
//! | ≤ 32 B（4 words） | 寄存器直传 | syscall 寄存器 rdi/rsi/rdx/r10 承载，零拷贝 |
//! | ≤ 4 KB | kmap 单拷贝 | 发送方物理页临时映射到 kmap 窗口 → memcpy 到接收方 |
//! | > 4 KB | 共享内存 grant | payload 只放描述符（MemoryRegion cap + offset），不拷数据 |

/// 寄存器直传阈值（Doc 03 §4.3：4 words = 32 bytes）。
pub const INLINE_THRESHOLD_BYTES: u32 = 32;

/// 单拷贝路径的 payload 上限（= 1 页；超过走 grant 描述符）。
pub const SINGLE_COPY_MAX_BYTES: u32 = 4096;

/// 传输路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferPath {
    /// 寄存器直传（fast path，目标 < 1 µs round-trip）。
    RegisterDirect,
    /// kmap 单拷贝（目标 < 3 µs round-trip，TBD 实测）。
    SingleCopy,
    /// 共享内存 grant（O(1)，只传描述符）。
    SharedGrant,
}

/// 按 payload 字节数分类传输路径（O(1) 两次比较，热路径无分支预测压力）。
///
/// 注意：`> 4KB` 的**原始数据**不进消息体——发送方必须改为携带
/// grant 描述符（描述符本身很小，实际走 [`TransferPath::RegisterDirect`]）；
/// 本函数对超限值返回 [`TransferPath::SharedGrant`] 供内核拒绝或引导。
pub const fn classify(payload_len: u32) -> TransferPath {
    if payload_len <= INLINE_THRESHOLD_BYTES {
        TransferPath::RegisterDirect
    } else if payload_len <= SINGLE_COPY_MAX_BYTES {
        TransferPath::SingleCopy
    } else {
        TransferPath::SharedGrant
    }
}
