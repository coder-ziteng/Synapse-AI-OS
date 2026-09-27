//! agent_id 命名空间注册表（对齐 [Doc 01 §7](../../../docs/design/01-capability-agent-permission-model.md)
//! + [Doc 02 §5.2](../../../docs/design/02-userspace-abi-and-process-model.md)）。
//!
//! 职责划分：
//! - **字符串 `agent_id` ↔ 数值 [`AgentId`] 映射由 init 进程（用户态）维护**；
//! - 内核侧只维护**数值 ID 唯一性注册表**（本模块）：spawn 时注册，
//!   重复 → [`CapError::AgentIdConflict`]（-6）；reap 时释放。

use synapse_cap::CapError;
use synapse_ipc::AgentId;

/// 注册表容量（与 [`crate::process::MAX_PROCS`] 一致即可，每进程一个 agent）。
pub const MAX_AGENTS: usize = 128;

/// 注册条目：数值 agent ↔ 持有它的进程。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentEntry {
    /// 数值 agent_id。
    pub agent: AgentId,
    /// 持有进程。
    pub owner: crate::process::Pid,
}

/// 内核侧 agent_id 唯一性注册表（固定数组，线性扫描——
/// spawn/reap 冷路径，MAX=128 可接受）。
pub struct AgentRegistry {
    entries: [Option<AgentEntry>; MAX_AGENTS],
    count: usize,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    /// 创建空注册表。
    pub fn new() -> AgentRegistry {
        AgentRegistry { entries: [const { None }; MAX_AGENTS], count: 0 }
    }

    /// 当前注册数。
    pub const fn len(&self) -> usize {
        self.count
    }

    /// 是否为空。
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 注册（spawn 路径）：数值 ID 已被占用 → [`CapError::AgentIdConflict`]；
    /// 表满 → [`CapError::NoMemory`]。`AgentId(0)`（未盖章哨兵）不可注册。
    pub fn register(&mut self, agent: AgentId, owner: crate::process::Pid) -> Result<(), CapError> {
        if agent == AgentId::UNSTAMPED {
            return Err(CapError::InvalidCap);
        }
        if self.lookup(agent).is_some() {
            return Err(CapError::AgentIdConflict);
        }
        for slot in self.entries.iter_mut() {
            if slot.is_none() {
                *slot = Some(AgentEntry { agent, owner });
                self.count += 1;
                return Ok(());
            }
        }
        Err(CapError::NoMemory)
    }

    /// 释放（reap 路径）：不存在为 no-op（进程可能未注册 agent 即夭折）。
    pub fn release(&mut self, agent: AgentId) {
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|e| e.agent == agent) {
                *slot = None;
                self.count -= 1;
                return;
            }
        }
    }

    /// 查询数值 ID → 持有进程。
    pub fn lookup(&self, agent: AgentId) -> Option<crate::process::Pid> {
        self.entries.iter().flatten().find(|e| e.agent == agent).map(|e| e.owner)
    }

    /// 按进程反查其 agent（进程退出清理 / 审计路径）。
    pub fn find_by_owner(&self, owner: crate::process::Pid) -> Option<AgentId> {
        self.entries.iter().flatten().find(|e| e.owner == owner).map(|e| e.agent)
    }
}
