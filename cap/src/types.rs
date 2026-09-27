//! 核心类型（对齐 [Doc 01 §4 / §4.1 / §4.2](../../../docs/design/01-capability-agent-permission-model.md)）。

use crate::rights::Rights;

/// Capability 引用：本进程 CapTable 槽位索引。
///
/// ★ 8-bit，对齐 Doc 01 §4 设计约束：per-process 256 槽上限，
/// IPC 消息中 cap transfer 仅占 1 byte / cap（NFR2）。
pub type CapRef = u8;

/// 每进程 CapTable 槽位上限（`CapRef = u8` 的表达范围）。
pub const CAP_TABLE_SIZE: usize = 256;

/// 内核对象引用 = slot index + generation。
///
/// ★ generation 防 slot 复用攻击（Doc 01 §4.2）：
/// slot 释放后再分配时 generation 递增，旧引用校验必然失败
/// （返回 [`crate::error::CapError::ObjectRetired`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjRef {
    /// 内核对象表索引。
    pub index: u32,
    /// 代际号：每次 slot 复用递增（wrap-around u32 足够）。
    pub generation: u32,
}

/// 内核对象生命周期状态（Doc 01 §4.2 状态机）。
///
/// ```text
/// Live ──► Revoking ──► Retired ──► Freed
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjState {
    /// 正常可用。
    Live,
    /// 撤销进行中（derivation tree 遍历）；新 invoke 立即拒绝，不等待遍历完成。
    Revoking,
    /// 已撤销，不可 invoke；等待引用计数归零后释放。
    Retired,
    /// 内存已释放（不应再被任何 ObjRef 命中）。
    Freed,
}

/// 内核对象类型全集（Doc 01 §2.1 基础四类 + §4.1 服务层扩展）。
///
/// 原则：内核只认识"对象 + 权限位"，不认识 L1~L5 或业务通道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    /// IPC 同步端点（SEND / RECV / REPLY）。
    Endpoint,
    /// 异步通知对象（位图语义，Doc 03 §6.2）。
    Notification,
    /// 内存区域（READ / WRITE / EXEC / GRANT）。
    MemoryRegion,
    /// MMIO 设备（MAP / IRQ_BIND）。
    Device,
    /// 进程（SCHED_SET / SIGNAL / DEBUG / ADMIN）。
    Process,
    /// 线程。
    Thread,
    /// 外交工具业务请求入口（Phase 5）。
    Diplomat,
    /// 审计日志（APPEND_KERNEL_EVENT / QUERY，Phase 4）。
    AuditLog,
    /// 业务持久化存储（Phase 6，必须经 Security Gateway）。
    Storage,
    /// WASM 沙箱运行时（S5）。
    WasmRuntime,
}

/// 能力对象 = 内核对象引用 + 权限位（Doc 01 §4）。
///
/// 设计约束：`Clone` 而非 `Copy` —— `parent` 字段追踪派生关系，
/// 拷贝时必须显式处理父子链。
#[derive(Clone, Debug)]
pub struct Capability {
    /// 指向内核对象（index + generation）。
    pub obj: ObjRef,
    /// 权限位掩码。
    pub rights: Rights,
    /// 可选：区分同一 endpoint 的不同调用者（seL4 badge 语义）。
    pub badge: u32,
    /// 父 capability 引用（Doc 01 §3.3 保留父子链，撤销级联的前提）。
    ///
    /// - `None` = 根 capability（由 init 铸造）；
    /// - `Some(cptr)` = 由本表内 `cptr` 槽的 capability 派生 / 转移而来；
    /// - 撤销时沿此字段遍历 derivation tree（Doc 01 §3.4）。
    pub parent: Option<CapRef>,
}

impl Capability {
    /// 构造根 capability（`parent = None`，由 init 铸造路径使用）。
    pub fn root(obj: ObjRef, rights: Rights) -> Capability {
        Capability { obj, rights, badge: 0, parent: None }
    }

    /// 派生一个权限子集 capability（attenuation-only，不可放大）。
    ///
    /// `mask` 与当前权限取交集，保证派生权限 ⊆ 原权限（Doc 01 §3.3）。
    /// `parent` 指向派生来源在本表中的槽位，由调用方在安装时填入。
    pub fn attenuated(&self, mask: Rights) -> Capability {
        Capability {
            obj: self.obj,
            rights: self.rights.intersection(mask),
            badge: self.badge,
            parent: None, // 由安装方设置
        }
    }
}
