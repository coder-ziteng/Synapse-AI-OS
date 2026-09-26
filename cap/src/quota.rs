//! 每进程资源配额（对齐 [Doc 02 §5.5](../../../docs/design/02-userspace-abi-and-process-model.md)）。
//!
//! Capability 模型解决"能不能做"，配额解决"能做多少"。
//! 超限一律返回 [`CapError::QuotaExceeded`]（errno -13），**不阻塞、不 panic**。
//!
//! 配额检查在分配路径上，O(1) 比较，不影响 NFR2。
//! 首期裁剪：不含 CPU 时间配额、不含网络请求配额（Doc 02 §5.5）。

use crate::error::CapError;

/// 每进程资源配额上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// 物理页帧上限（含内核映射）。
    pub max_pages: u32,
    /// 线程数上限。
    pub max_threads: u16,
    /// CapTable 槽位上限（≤ 256，构造时校验）。
    pub max_caps: u16,
    /// 持有的 Endpoint 对象数上限。
    pub max_endpoints: u16,
    /// 单条 IPC 消息最大字节数（≤ 4096）。
    pub max_msg_size: u32,
    /// 未完成 IPC 请求数上限。
    pub max_pending_ipc: u16,
    /// 共享内存 grant 数上限。
    pub max_grants: u16,
}

/// 默认配额（init 进程 spawn 子进程时的初始值，Doc 02 §5.5）。
pub const DEFAULT_QUOTA: Quota = Quota {
    max_pages: 4096,       // 16 MB
    max_threads: 16,
    max_caps: 64,          // 保守值，远低于 256 上限
    max_endpoints: 8,
    max_msg_size: 4096,    // 4 KB
    max_pending_ipc: 32,
    max_grants: 8,
};

/// 单条 IPC 消息字节数硬上限（Doc 03 §4：4KB 内联）。
pub const MAX_MSG_SIZE: u32 = 4096;

impl Quota {
    /// 校验配额自身合法性（`max_caps ≤ 256`、`max_msg_size ≤ 4096`）。
    ///
    /// 非法配额 → [`CapError::QuotaExceeded`]（拒绝构造，防 ABI 漂移）。
    pub fn validate(&self) -> Result<(), CapError> {
        if (self.max_caps as usize) > crate::types::CAP_TABLE_SIZE
            || self.max_msg_size > MAX_MSG_SIZE
        {
            return Err(CapError::QuotaExceeded);
        }
        Ok(())
    }
}

/// 可计费资源类型（配额核算维度）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// 物理页帧。
    Pages,
    /// 线程。
    Threads,
    /// CapTable 槽位。
    Caps,
    /// Endpoint 对象。
    Endpoints,
    /// 未完成 IPC 请求。
    PendingIpc,
    /// 共享内存 grant。
    Grants,
}

/// 每进程资源用量计数器（FR8：计数器 ≤ 配额 → 允许；> 配额 → 拒绝）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuotaUsage {
    pages: u32,
    threads: u16,
    caps: u16,
    endpoints: u16,
    pending_ipc: u16,
    grants: u16,
}

impl QuotaUsage {
    /// 创建空计数器。
    pub const fn new() -> QuotaUsage {
        QuotaUsage {
            pages: 0,
            threads: 0,
            caps: 0,
            endpoints: 0,
            pending_ipc: 0,
            grants: 0,
        }
    }

    /// 当前用量。
    pub const fn used(&self, res: Resource) -> u32 {
        match res {
            Resource::Pages => self.pages,
            Resource::Threads => self.threads as u32,
            Resource::Caps => self.caps as u32,
            Resource::Endpoints => self.endpoints as u32,
            Resource::PendingIpc => self.pending_ipc as u32,
            Resource::Grants => self.grants as u32,
        }
    }

    fn limit_of(quota: &Quota, res: Resource) -> u32 {
        match res {
            Resource::Pages => quota.max_pages,
            Resource::Threads => quota.max_threads as u32,
            Resource::Caps => quota.max_caps as u32,
            Resource::Endpoints => quota.max_endpoints as u32,
            Resource::PendingIpc => quota.max_pending_ipc as u32,
            Resource::Grants => quota.max_grants as u32,
        }
    }

    /// 计费：`used + amount ≤ limit` 才放行，否则
    /// [`CapError::QuotaExceeded`]（不阻塞、不 panic，Doc 02 §5.5）。
    ///
    /// `amount = 0` 恒成功；`u16` 维度溢出同样按超限处理。
    pub fn charge(
        &mut self,
        quota: &Quota,
        res: Resource,
        amount: u32,
    ) -> Result<(), CapError> {
        let used = self.used(res);
        let limit = Self::limit_of(quota, res);
        let next = used.checked_add(amount).ok_or(CapError::QuotaExceeded)?;
        if next > limit {
            return Err(CapError::QuotaExceeded);
        }
        match res {
            Resource::Pages => self.pages = next,
            Resource::Threads => self.threads = next as u16,
            Resource::Caps => self.caps = next as u16,
            Resource::Endpoints => self.endpoints = next as u16,
            Resource::PendingIpc => self.pending_ipc = next as u16,
            Resource::Grants => self.grants = next as u16,
        }
        Ok(())
    }

    /// 释放计费（对象销毁 / 进程退出回收路径）。
    ///
    /// 饱和减法：release 多于 charge 属调用方 bug，计数器归零而不回绕。
    pub fn release(&mut self, res: Resource, amount: u32) {
        match res {
            Resource::Pages => self.pages = self.pages.saturating_sub(amount),
            Resource::Threads => {
                self.threads = self.threads.saturating_sub(amount as u16)
            }
            Resource::Caps => self.caps = self.caps.saturating_sub(amount as u16),
            Resource::Endpoints => {
                self.endpoints = self.endpoints.saturating_sub(amount as u16)
            }
            Resource::PendingIpc => {
                self.pending_ipc = self.pending_ipc.saturating_sub(amount as u16)
            }
            Resource::Grants => self.grants = self.grants.saturating_sub(amount as u16),
        }
    }

    /// IPC 消息大小检查（`size ≤ min(max_msg_size, 4096)`）。
    pub fn check_msg_size(&self, quota: &Quota, size: u32) -> Result<(), CapError> {
        if size > quota.max_msg_size || size > MAX_MSG_SIZE {
            return Err(CapError::QuotaExceeded);
        }
        Ok(())
    }
}

/// spawn 配额划拨检查（Doc 02 §5.5 规则"授予"）：
/// 子进程配额各项不得超过父进程**剩余**配额，否则拒绝 spawn。
///
/// 返回子进程初始 [`QuotaUsage`]（全零）——实际划拨由内核集成层
/// 在父进程账上 charge；本函数只做纯校验，无副作用。
pub fn check_spawn_grant(
    parent_quota: &Quota,
    parent_usage: &QuotaUsage,
    child_quota: &Quota,
) -> Result<(), CapError> {
    child_quota.validate()?;
    for res in [
        Resource::Pages,
        Resource::Threads,
        Resource::Caps,
        Resource::Endpoints,
        Resource::PendingIpc,
        Resource::Grants,
    ] {
        let remaining = QuotaUsage::limit_of(parent_quota, res)
            .saturating_sub(parent_usage.used(res));
        let need = QuotaUsage::limit_of(child_quota, res);
        if need > remaining {
            return Err(CapError::QuotaExceeded);
        }
    }
    Ok(())
}
