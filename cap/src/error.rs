//! 统一错误码（对齐 [Doc 02 §4.3 / §4.5](../../../docs/design/02-userspace-abi-and-process-model.md)）。
//!
//! 约定：syscall 返回值为负表示错误（`-1..=-127`），非负表示成功。
//! 本模块定义 capability 子系统使用的错误子集及其 errno 映射。
//!
//! 语义消歧（实现层约定，文档已同步）：
//! - [`CapError::InvalidCap`]：引用本身无效（cptr 越界 / 空槽 / 对象索引越界）；
//! - [`CapError::Permission`]：引用有效但权限位不覆盖本次操作；
//! - [`CapError::ObjectRetired`]：引用曾有效但对象已撤销 / generation 不匹配。

/// Capability 子系统错误。
///
/// 与 syscall ABI 错误码一一对应（[`CapError::errno`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapError {
    /// capability 引用无效（cptr 越界 / 空槽 / 对象索引越界）。errno = -1
    InvalidCap,
    /// 用户态地址非法（未映射 / 权限不足 / 对齐错误）。errno = -2
    InvalidAddr,
    /// 内核内存不足（页帧 / CapTable 槽位）。errno = -3
    NoMemory,
    /// 非阻塞操作无法立即完成。errno = -4
    WouldBlock,
    /// 对象不存在（pid / agent_id / endpoint）。errno = -5
    NotFound,
    /// agent_id 重复。errno = -6
    AgentIdConflict,
    /// 权限不足（rights 位不覆盖本次操作）。errno = -7
    Permission,
    /// 目标进程已冻结。errno = -8
    Frozen,
    /// 目标进程已退出（需先 reap）。errno = -9
    Zombie,
    /// syscall 未实现（预留）。errno = -10
    NotImplemented,
    /// 用户态与内核 ABI 版本不兼容。errno = -11
    AbiMismatch,
    /// 对象已撤销 / 退休（generation 不匹配）。errno = -12
    ObjectRetired,
    /// 进程资源配额耗尽。errno = -13
    QuotaExceeded,
    /// IPC 对端进程已退出。errno = -14
    PeerDied,
    /// 操作超时。errno = -15
    Timeout,
}

impl CapError {
    /// 映射为 syscall ABI 错误码（负值）。
    ///
    /// 对齐 Doc 02 §4.3（-1..-10）与 §4.5（-11..-15）。
    pub fn errno(self) -> i32 {
        match self {
            CapError::InvalidCap => -1,
            CapError::InvalidAddr => -2,
            CapError::NoMemory => -3,
            CapError::WouldBlock => -4,
            CapError::NotFound => -5,
            CapError::AgentIdConflict => -6,
            CapError::Permission => -7,
            CapError::Frozen => -8,
            CapError::Zombie => -9,
            CapError::NotImplemented => -10,
            CapError::AbiMismatch => -11,
            CapError::ObjectRetired => -12,
            CapError::QuotaExceeded => -13,
            CapError::PeerDied => -14,
            CapError::Timeout => -15,
        }
    }
}
