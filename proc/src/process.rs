//! 进程控制块与生命周期状态机（对齐 [Doc 02 §5.2/§5.3](../../../docs/design/02-userspace-abi-and-process-model.md)）。
//!
//! ```text
//! Created ──► Running ⇄ Blocked
//!                │  ╲        ╱  │
//!                ▼   ▼      ▼   ▼
//!              Exited / Faulted        （进程崩溃 → Faulted）
//!                │        │
//!                ▼        ▼            release_resources：页帧 → CapTable
//!                  Zombie              （保留 pid + 退出码）
//!                    │
//!                    ▼                 reap：agent_id → pid
//!                  (释放)
//! ```
//!
//! 资源释放顺序（Doc 02 §5.3）：**页帧 → CapTable → agent_id → pid**
//! （前两步在 [`ProcessTable::release_resources`]，后两步在 [`ProcessTable::reap`]）。
//!
//! 本模块建模纯状态转移；调度、ELF 加载、页帧/CapTable 的实际释放
//! 由内核集成层执行（返回的 [`DeathSignal`] / 状态即集成层的行动指令）。

use crate::agent::AgentRegistry;
use synapse_cap::quota::{check_spawn_grant, Quota, QuotaUsage, Resource};
use synapse_cap::{CapError, CapRef};
use synapse_ipc::AgentId;

/// 进程表容量（首期固定；pid 单调分配不复用，槽满 → NoMemory）。
pub const MAX_PROCS: usize = 128;

/// 进程 ID（内核数值形态；`0` 保留为 NULL 哨兵，对齐 CapTable slot 0 语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pid(pub u32);

impl Pid {
    /// NULL 哨兵（永不分配给真实进程）。
    pub const NULL: Pid = Pid(0);
}

/// init 进程固定 pid（Doc 02 §5.2：首期仅 init 可 spawn；§5.3：init 是默认收尸人）。
pub const INIT_PID: Pid = Pid(1);

/// init 进程的全局初始预算（Doc 02 §5.5"init 预算"：内核启动时设定，不可被其他进程修改）。
pub const INIT_QUOTA: Quota = Quota {
    max_pages: 1 << 20, // 4 GB
    max_threads: 64,
    max_caps: 256,
    max_endpoints: 64,
    max_msg_size: 4096,
    max_pending_ipc: 256,
    max_grants: 64,
};

/// 进程生命周期状态（Doc 02 §5.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// 已创建，等待调度器首次 admitting。
    Created,
    /// 运行中。
    Running,
    /// 阻塞（IPC wait / Notification wait 等）。
    Blocked,
    /// 正常退出（`process_exit`），等待资源释放。
    Exited,
    /// 崩溃（段错误 / panic / 非法 syscall），等待资源释放。
    Faulted,
    /// 页帧 + CapTable 已释放，保留 pid + 退出码，等待父进程 `reap`。
    Zombie,
}

impl ProcState {
    /// 是否占用调度资源（Created/Running/Blocked）。
    pub const fn is_live(self) -> bool {
        matches!(self, ProcState::Created | ProcState::Running | ProcState::Blocked)
    }

    /// 是否已终止（Exited/Faulted/Zombie）。
    pub const fn is_dead(self) -> bool {
        !self.is_live()
    }
}

/// 崩溃原因（death signal 携带，供监督树决策，Doc 02 §5.3/§5.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// 段错误（访问未映射 / 权限不符地址，Doc 02 §3.3）。
    SegFault,
    /// 用户态 panic。
    Panic,
    /// 非法 syscall（未知号 / 参数越界）。
    IllegalSyscall,
    /// 非法指令。
    IllegalInstruction,
    /// 一般保护异常。
    GeneralProtection,
}

/// death notification 消息（Doc 02 §5.3：投递到父进程注册的 death endpoint，
/// 复用 IPC 机制，无需新原语）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeathSignal {
    /// 终止的进程。
    pub pid: Pid,
    /// 退出码（Faulted 时为内核填充的异常码）。
    pub exit_code: i32,
    /// 崩溃原因（正常退出为 None）。
    pub fault_reason: Option<FaultKind>,
}

/// 进程控制块。
#[derive(Clone, Debug)]
pub struct Pcb {
    /// 进程 ID。
    pub pid: Pid,
    /// 生命周期状态。
    pub state: ProcState,
    /// 数值 agent_id（内核盖章来源，Doc 01 §7）。
    pub agent: AgentId,
    /// 父进程（孤儿过继后可能变为 [`INIT_PID`]）。
    pub parent: Pid,
    /// spawn 时注册的 death endpoint（指向父进程持有的 Endpoint，Doc 02 §5.3）。
    pub death_endpoint: CapRef,
    /// 配额上限（spawn 时从父进程划拨）。
    pub quota: Quota,
    /// 资源用量计数器（FR8）。
    pub usage: QuotaUsage,
    /// 行为围栏冻结标记（Doc 02 §5.4；正交于生命周期状态）。
    pub frozen: bool,
    /// 退出码（Exited/Faulted 后有效）。
    pub exit_code: i32,
    /// 崩溃原因（Faulted 后有效）。
    pub fault_reason: Option<FaultKind>,
}

/// spawn 参数（Doc 02 §5.2 `process_spawn` 的纯数据部分；
/// ELF 加载与初始 caps 安装分别由内核集成层 / [`crate::grant`] 负责）。
#[derive(Clone, Copy, Debug)]
pub struct SpawnParams {
    /// 子进程数值 agent_id（父进程指定，注册表查重）。
    pub agent: AgentId,
    /// 子进程初始配额（从父进程剩余中划拨，不可超过）。
    pub quota: Quota,
    /// 父进程持有的 death endpoint（子进程终止时向其投递 [`DeathSignal`]）。
    pub death_endpoint: CapRef,
}

/// 全局进程表（内核集成层持有一份，Spinlock + 关中断保护）。
pub struct ProcessTable {
    slots: [Option<Pcb>; MAX_PROCS],
    /// 单调递增 pid 分配器（不复用，防陈旧 pid 混淆；槽位 = pid % MAX_PROCS）。
    next_pid: u32,
    /// agent_id 唯一性注册表（spawn 注册 / reap 释放）。
    pub agents: AgentRegistry,
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new(AgentId(1))
    }
}

impl ProcessTable {
    /// 创建进程表并安装 init 进程（pid = [`INIT_PID`]，Running，
    /// 配额 = [`INIT_QUOTA`] 全局初始预算）。
    pub fn new(init_agent: AgentId) -> ProcessTable {
        let mut t = ProcessTable {
            slots: [const { None }; MAX_PROCS],
            next_pid: INIT_PID.0 + 1,
            agents: AgentRegistry::new(),
        };
        t.slots[INIT_PID.0 as usize] = Some(Pcb {
            pid: INIT_PID,
            state: ProcState::Running,
            agent: init_agent,
            parent: Pid::NULL,
            death_endpoint: 0,
            quota: INIT_QUOTA,
            usage: QuotaUsage::new(),
            frozen: false,
            exit_code: 0,
            fault_reason: None,
        });
        // init 的 agent 也入注册表（保持"每活进程一注册"不变量）
        let _ = t.agents.register(init_agent, INIT_PID);
        t
    }

    /// 查 PCB（不存在 / 已 reap → None）。
    pub fn get(&self, pid: Pid) -> Option<&Pcb> {
        let idx = (pid.0 % MAX_PROCS as u32) as usize;
        self.slots[idx].as_ref().filter(|p| p.pid == pid)
    }

    /// 可变查 PCB（内核集成层内部路径）。
    pub fn get_mut(&mut self, pid: Pid) -> Option<&mut Pcb> {
        let idx = (pid.0 % MAX_PROCS as u32) as usize;
        self.slots[idx].as_mut().filter(|p| p.pid == pid)
    }

    /// 存活进程数（含 Zombie）。
    pub fn live_count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    // ---------- spawn（Doc 02 §5.2） ----------

    /// 创建子进程。校验链（任一失败零副作用）：
    ///
    /// 1. 父进程存在 → 否则 [`CapError::NotFound`]；已终止 → [`CapError::Zombie`]；
    /// 2. 父进程未冻结 → 否则 [`CapError::Frozen`]；
    /// 3. **首期仅 init 可 spawn** → 否则 [`CapError::Permission`]
    ///    （Phase 5+ 放开至持有 PROCESS::SPAWN capability 的进程）；
    /// 4. 子配额合法且 ≤ 父剩余（划拨语义）→ 否则 [`CapError::QuotaExceeded`]；
    /// 5. agent_id 唯一 → 否则 [`CapError::AgentIdConflict`]；
    /// 6. pid 槽可用 → 否则 [`CapError::NoMemory`]。
    ///
    /// 成功后：子进程为 `Created`；父进程 usage 按子配额上限**预留划拨**
    /// （reap 时归还，Doc 02 §5.5"回收"）。
    pub fn spawn(&mut self, parent: Pid, params: SpawnParams) -> Result<Pid, CapError> {
        let p = self.get(parent).ok_or(CapError::NotFound)?;
        if p.state.is_dead() {
            return Err(CapError::Zombie);
        }
        if p.frozen {
            return Err(CapError::Frozen);
        }
        if parent != INIT_PID {
            return Err(CapError::Permission); // 首期限制（Doc 02 §5.2）
        }
        params.quota.validate()?;
        let parent_quota = p.quota;
        let parent_usage = p.usage;
        check_spawn_grant(&parent_quota, &parent_usage, &params.quota)?;

        // agent 查重（先查后注册，pid 分配失败不留脏注册）
        if self.agents.lookup(params.agent).is_some() {
            return Err(CapError::AgentIdConflict);
        }

        // pid 分配：单调递增，槽位 = pid % MAX_PROCS，槽被占（未 reap 完）→ NoMemory
        let pid = Pid(self.next_pid);
        let idx = (pid.0 % MAX_PROCS as u32) as usize;
        if self.slots[idx].is_some() {
            return Err(CapError::NoMemory);
        }

        // 全部检查通过 → 提交副作用
        self.next_pid += 1;
        self.agents
            .register(params.agent, pid)
            .map_err(|_| CapError::AgentIdConflict)?;
        self.slots[idx] = Some(Pcb {
            pid,
            state: ProcState::Created,
            agent: params.agent,
            parent,
            death_endpoint: params.death_endpoint,
            quota: params.quota,
            usage: QuotaUsage::new(),
            frozen: false,
            exit_code: 0,
            fault_reason: None,
        });
        // 配额划拨：按子上限在父账上预留（reap 时归还）
        let pa = self.get_mut(parent).expect("parent checked");
        carve(&mut pa.usage, &pa.quota, &params.quota);
        Ok(pid)
    }

    // ---------- 调度状态（内核集成层驱动） ----------

    /// `Created/Blocked → Running`（调度器 admit / 唤醒）。
    pub fn set_running(&mut self, pid: Pid) -> Result<(), CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        match p.state {
            ProcState::Created | ProcState::Blocked => {
                p.state = ProcState::Running;
                Ok(())
            }
            ProcState::Running => Ok(()), // 幂等
            ProcState::Exited | ProcState::Faulted | ProcState::Zombie => {
                Err(CapError::Zombie)
            }
        }
    }

    /// `Running → Blocked`（IPC / Notification 等待）。
    pub fn set_blocked(&mut self, pid: Pid) -> Result<(), CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        if p.state.is_dead() {
            return Err(CapError::Zombie);
        }
        p.state = ProcState::Blocked;
        Ok(())
    }

    // ---------- 终止（Doc 02 §5.3） ----------

    /// 正常退出（`process_exit` syscall）：live → `Exited`，
    /// 孤儿过继给 init，返回待投递的 [`DeathSignal`]
    /// （集成层向 `death_endpoint` 走常规 IPC 投递）。
    pub fn exit(&mut self, pid: Pid, code: i32) -> Result<DeathSignal, CapError> {
        self.terminate(pid, ProcState::Exited, code, None)
    }

    /// 崩溃（段错误 / panic / 非法 syscall）：live → `Faulted`，
    /// 其余语义同 [`Self::exit`]。
    pub fn fault(
        &mut self,
        pid: Pid,
        kind: FaultKind,
        code: i32,
    ) -> Result<DeathSignal, CapError> {
        self.terminate(pid, ProcState::Faulted, code, Some(kind))
    }

    fn terminate(
        &mut self,
        pid: Pid,
        state: ProcState,
        code: i32,
        kind: Option<FaultKind>,
    ) -> Result<DeathSignal, CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        if !p.state.is_live() {
            return Err(CapError::Zombie); // 已终止，需先 reap
        }
        p.state = state;
        p.exit_code = code;
        p.fault_reason = kind;
        p.frozen = false; // 冻结随终止解除
        let signal = DeathSignal { pid, exit_code: code, fault_reason: kind };

        // 孤儿过继：该进程的所有子进程 parent → init（Doc 02 §5.3）
        if pid != INIT_PID {
            for slot in self.slots.iter_mut().flatten() {
                if slot.parent == pid {
                    slot.parent = INIT_PID;
                }
            }
        }
        Ok(signal)
    }

    /// 资源释放（Exited/Faulted → Zombie）：集成层在此**之前**已按顺序
    /// 释放页帧 → CapTable（Doc 02 §5.3 资源释放顺序前两步）。
    pub fn release_resources(&mut self, pid: Pid) -> Result<(), CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        match p.state {
            ProcState::Exited | ProcState::Faulted => {
                p.state = ProcState::Zombie;
                Ok(())
            }
            ProcState::Zombie => Ok(()), // 幂等
            ProcState::Created | ProcState::Running | ProcState::Blocked => {
                Err(CapError::Permission) // 活进程不可释放资源
            }
        }
    }

    /// 收尸（`reap` syscall）：Zombie → 槽释放 + agent_id 释放
    /// （资源释放顺序后两步），并把 spawn 时划拨给父进程的配额预留归还。
    ///
    /// 权限：`reaper` 必须是其父进程（孤儿过继后即 init）→ 否则
    /// [`CapError::Permission`]；目标非 Zombie → [`CapError::Zombie`]
    /// （活进程不可收尸 → [`CapError::Permission`]）。
    pub fn reap(&mut self, reaper: Pid, pid: Pid) -> Result<(), CapError> {
        let (parent, quota, agent, state) = {
            let p = self.get(pid).ok_or(CapError::NotFound)?;
            (p.parent, p.quota, p.agent, p.state)
        };
        if state == ProcState::Zombie {
            if reaper != parent && reaper != INIT_PID {
                return Err(CapError::Permission);
            }
        } else if state.is_dead() {
            return Err(CapError::Zombie); // 尚未 release_resources
        } else {
            return Err(CapError::Permission); // 活进程不可收尸
        }

        // 释放：agent_id → pid（页帧/CapTable 已在 release_resources 前完成）
        self.agents.release(agent);
        let idx = (pid.0 % MAX_PROCS as u32) as usize;
        self.slots[idx] = None;
        // 配额预留归还父进程（父已亡则随过继记在 init 账上，见 TBD）
        if let Some(pa) = self.get_mut(parent) {
            uncarve(&mut pa.usage, &quota);
        }
        Ok(())
    }

    // ---------- 行为围栏（Doc 02 §5.4） ----------

    /// 冻结进程（需 PROCESS::ADMIN capability——syscall 层校验）。
    /// 冻结后不可调度；向其发 IPC → [`CapError::Frozen`]（-8）。
    pub fn freeze(&mut self, pid: Pid) -> Result<(), CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        if p.state.is_dead() {
            return Err(CapError::Zombie);
        }
        p.frozen = true;
        Ok(())
    }

    /// 解冻进程。
    pub fn thaw(&mut self, pid: Pid) -> Result<(), CapError> {
        let p = self.get_mut(pid).ok_or(CapError::NotFound)?;
        if p.state.is_dead() {
            return Err(CapError::Zombie);
        }
        p.frozen = false;
        Ok(())
    }

    /// IPC / 调度入站检查：目标必须存活且未冻结。
    ///
    /// - 不存在 → [`CapError::NotFound`]；
    /// - 已终止 → [`CapError::Zombie`]（-9）；
    /// - 冻结 → [`CapError::Frozen`]（-8）。
    pub fn ensure_accepts_ipc(&self, pid: Pid) -> Result<(), CapError> {
        let p = self.get(pid).ok_or(CapError::NotFound)?;
        if p.state.is_dead() {
            return Err(CapError::Zombie);
        }
        if p.frozen {
            return Err(CapError::Frozen);
        }
        Ok(())
    }
}

/// spawn 配额划拨：按子配额**上限**在父账上预留各维度用量。
///
/// 调用前 [`check_spawn_grant`] 已保证每维 `child.max ≤ parent 剩余`，
/// 因此此处 charge 必然成功（防御性忽略错误，不 panic）。
fn carve(parent_usage: &mut QuotaUsage, parent_quota: &Quota, child: &Quota) {
    let _ = parent_usage.charge(parent_quota, Resource::Pages, child.max_pages);
    let _ = parent_usage.charge(parent_quota, Resource::Threads, child.max_threads as u32);
    let _ = parent_usage.charge(parent_quota, Resource::Caps, child.max_caps as u32);
    let _ = parent_usage.charge(parent_quota, Resource::Endpoints, child.max_endpoints as u32);
    let _ = parent_usage.charge(parent_quota, Resource::PendingIpc, child.max_pending_ipc as u32);
    let _ = parent_usage.charge(parent_quota, Resource::Grants, child.max_grants as u32);
}

/// reap 配额归还（[`carve`] 的逆操作，饱和减法防回绕）。
fn uncarve(parent_usage: &mut QuotaUsage, child: &Quota) {
    parent_usage.release(Resource::Pages, child.max_pages);
    parent_usage.release(Resource::Threads, child.max_threads as u32);
    parent_usage.release(Resource::Caps, child.max_caps as u32);
    parent_usage.release(Resource::Endpoints, child.max_endpoints as u32);
    parent_usage.release(Resource::PendingIpc, child.max_pending_ipc as u32);
    parent_usage.release(Resource::Grants, child.max_grants as u32);
}

/// 默认配额再导出（集成层构造 [`SpawnParams`] 便利）。
pub use synapse_cap::DEFAULT_QUOTA as SPAWN_DEFAULT_QUOTA;
