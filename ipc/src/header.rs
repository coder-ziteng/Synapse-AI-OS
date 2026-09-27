//! IPC 消息头（对齐 [Doc 03 §3.1](../../../docs/design/03-ipc-message-and-single-copy-path.md)
//! + [Doc 02 §4.5](../../../docs/design/02-userspace-abi-and-process-model.md) ABI 版本策略）。
//!
//! 布局为 `#[repr(C)]` 固定 ABI；`version` + `header_len` 支持向前兼容
//! （接收方按 `header_len` 跳过未知尾部字段；version 不匹配 → `E_ABI_MISMATCH`）。

use synapse_cap::quota::Quota;
use synapse_cap::{CapError, MAX_TRANSFER};

/// 当前内核↔用户态 IPC 消息头 ABI 版本（Doc 02 §4.5：不兼容变更须递增）。
pub const IPC_ABI_VERSION: u8 = 1;

/// 消息体最大字节数（Doc 03 §4.3：4KB = 1 页；超过走共享内存 grant）。
pub const MAX_PAYLOAD: u32 = 4096;

/// `header_len` 允许上限（防恶意超大头；当前头部 20 字节，余量给未来追加字段）。
pub const MAX_HEADER_LEN: u16 = 256;

/// 发送方 Agent 数值 ID（内核在发送路径盖章，用户态不可写，Doc 01 §7）。
///
/// 字符串 `agent_id` ↔ 数值 ID 的命名空间映射由 init 进程维护；
/// 内核与消息头只承载数值形态。`0` 保留为"未盖章"哨兵。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentId(pub u32);

impl AgentId {
    /// 未盖章哨兵值：用户态构造的消息头一律为此值，
    /// 内核发送路径必须调用 [`IpcHeader::stamp`] 覆盖。
    pub const UNSTAMPED: AgentId = AgentId(0);
}

/// IPC 消息头（固定 20 字节，`repr(C)`）。
///
/// 字段顺序经过对齐排布（1+1+1+1+2+4+4+4），无隐式 padding。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IpcHeader {
    /// ABI 版本（当前 = [`IPC_ABI_VERSION`]）。
    pub version: u8,
    /// 随消息转移的 capability 数量（0..=[`MAX_TRANSFER`]）。
    pub cap_transfer_count: u8,
    /// 保留位（MVP 严格校验：必须为 0，防 ABI 漂移）。
    pub flags: u8,
    /// 对齐填充（未使用）。
    pub _pad: u8,
    /// 头部总字节数（接收方据此跳过未知尾部字段）。
    pub header_len: u16,
    /// 用户自定义标签（区分同一 endpoint 的请求类型）。
    pub label: u32,
    /// 发送方 Agent（★ 内核盖章，用户态写无效）。
    pub sender_agent: AgentId,
    /// 消息体字节数（≤ [`MAX_PAYLOAD`]）。
    pub payload_len: u32,
}

impl IpcHeader {
    /// 用户态构造：version / header_len 自动填充，sender 为
    /// [`AgentId::UNSTAMPED`]（等待内核盖章）。
    pub const fn new(label: u32, payload_len: u32, cap_transfer_count: u8) -> IpcHeader {
        IpcHeader {
            version: IPC_ABI_VERSION,
            cap_transfer_count,
            flags: 0,
            _pad: 0,
            header_len: core::mem::size_of::<IpcHeader>() as u16,
            label,
            sender_agent: AgentId::UNSTAMPED,
            payload_len,
        }
    }

    /// 内核发送路径盖章（唯一合法的 sender_agent 写入点，防伪造）。
    pub fn stamp(&mut self, agent: AgentId) {
        self.sender_agent = agent;
    }

    /// 内核入站校验（syscall 边界一次性完成，O(1)）：
    ///
    /// - version 不匹配 / header_len 越界 / flags 非零 → [`CapError::AbiMismatch`]（-11）；
    /// - `cap_transfer_count > MAX_TRANSFER` → [`CapError::InvalidCap`]（-1）；
    /// - `payload_len` 超过 `min(quota.max_msg_size, 4KB)` → [`CapError::QuotaExceeded`]（-13）。
    pub fn validate(&self, quota: &Quota) -> Result<(), CapError> {
        if self.version != IPC_ABI_VERSION
            || (self.header_len as usize) < core::mem::size_of::<IpcHeader>()
            || self.header_len > MAX_HEADER_LEN
            || self.flags != 0
        {
            return Err(CapError::AbiMismatch);
        }
        if self.cap_transfer_count as usize > MAX_TRANSFER {
            return Err(CapError::InvalidCap);
        }
        // payload 上限 = min(进程配额 max_msg_size, 内核硬上限 4KB)
        if self.payload_len > quota.max_msg_size || self.payload_len > MAX_PAYLOAD {
            return Err(CapError::QuotaExceeded);
        }
        Ok(())
    }
}
