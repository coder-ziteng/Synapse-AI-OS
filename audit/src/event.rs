//! 审计事件（对齐 [Doc 03 §6.3](../../../docs/design/03-ipc-message-and-single-copy-path.md) FR9）。
//!
//! **不可篡改 + 不泄密**双重约束：
//! - 事件由内核在事件点直接构造（`actor` agent_id 已盖章，伪造无意义）；
//! - **永不包含用户态业务数据原文**——只含对象引用、权限位、`agent_id`、
//!   label 等元信息（Doc 01 §5.2 泄密防线）。
//!
//! 事件记录为固定大小、`repr(C)`、可跨 IPC 批量提交给审计服务（S5）。

use synapse_cap::{ObjKind, ObjRef, Rights};
use synapse_ipc::AgentId;

/// 事件大类（对应 Doc 03 §6.3 事件点表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    /// capability 校验（成功 / 失败）。
    CapVerify = 1,
    /// capability 生命周期（mint / destroy / delegate / revoke）。
    CapLifecycle = 2,
    /// IPC 关键路径（send / recv / reply，不含 payload 内容）。
    Ipc = 3,
    /// 进程生命周期（spawn / exit / fault / freeze / thaw）。
    Process = 4,
    /// 审计服务自身系统事件（启动 / 配置变更）。
    System = 5,
}

/// capability 生命周期操作子类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CapOp {
    /// 铸造（init 路径）。
    Mint = 0,
    /// 销毁。
    Destroy = 1,
    /// 委托（派生子 capability）。
    Delegate = 2,
    /// 撤销（derivation tree 级联）。
    Revoke = 3,
}

/// IPC 方向子类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IpcDir {
    /// 发送。
    Send = 0,
    /// 接收。
    Recv = 1,
    /// 回复。
    Reply = 2,
}

/// 进程生命周期操作子类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcOp {
    /// 创建。
    Spawn = 0,
    /// 正常退出。
    Exit = 1,
    /// 崩溃。
    Fault = 2,
    /// 冻结（行为围栏）。
    Freeze = 3,
    /// 解冻。
    Thaw = 4,
}

/// 审计服务系统事件子类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SystemEvent {
    /// 审计服务启动。
    Startup = 0,
    /// 配置变更。
    ConfigChange = 1,
}

/// 事件详情（按 [`EventKind`] 判别；固定大小，无堆分配）。
///
/// `pid` 用裸 `u32`（不复用 `synapse_proc::Pid`）以保持 audit crate
/// 依赖最小；语义即进程 ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDetail {
    /// capability 校验结果：目标对象 + 权限位 + 是否放行。
    CapVerify {
        /// 目标内核对象引用。
        target: ObjRef,
        /// 本次操作要求的权限位。
        rights: Rights,
        /// 校验是否通过。
        ok: bool,
    },
    /// capability 生命周期：操作 + 对象类型 + 来源/去向 agent。
    CapLifecycle {
        /// 具体操作。
        op: CapOp,
        /// 对象类型。
        obj: ObjKind,
        /// 来源 agent（mint 时为铸造者）。
        source: AgentId,
        /// 去向 agent（destroy/revoke 时与 source 相同）。
        dest: AgentId,
    },
    /// IPC 关键路径：方向 + endpoint + label（**不含 payload**）。
    Ipc {
        /// 方向。
        dir: IpcDir,
        /// endpoint 对象引用。
        endpoint: ObjRef,
        /// 用户标签（区分请求类型；非业务数据）。
        label: u32,
    },
    /// 进程生命周期：操作 + agent + 父进程 + 退出码/错误。
    Process {
        /// 具体操作。
        op: ProcOp,
        /// 目标进程 agent。
        agent: AgentId,
        /// 父进程 pid（spawn/exit 有意义；无则 `u32::MAX`）。
        parent_pid: u32,
        /// 退出码 / fault 编号 / errno。
        code: i32,
    },
    /// 审计服务系统事件。
    System {
        /// 事件子类型。
        event: SystemEvent,
        /// 附加参数（启动参数哈希 / 配置版本号）。
        param: u32,
    },
}

/// 单条审计事件（固定大小记录）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// 事件大类。
    pub kind: EventKind,
    /// 内核时间戳（TSC / 单调钟 tick；集成层填充，宿主测试可置 0）。
    pub timestamp: u64,
    /// 触发者 agent（★ 内核盖章，用户态不可伪造）。
    pub actor: AgentId,
    /// 事件详情。
    pub detail: EventDetail,
}

impl AuditEvent {
    /// 构造 capability 校验事件。
    pub fn cap_verify(
        actor: AgentId,
        target: ObjRef,
        rights: Rights,
        ok: bool,
        timestamp: u64,
    ) -> AuditEvent {
        AuditEvent {
            kind: EventKind::CapVerify,
            timestamp,
            actor,
            detail: EventDetail::CapVerify { target, rights, ok },
        }
    }

    /// 构造 capability 生命周期事件。
    pub fn cap_lifecycle(
        actor: AgentId,
        op: CapOp,
        obj: ObjKind,
        source: AgentId,
        dest: AgentId,
        timestamp: u64,
    ) -> AuditEvent {
        AuditEvent {
            kind: EventKind::CapLifecycle,
            timestamp,
            actor,
            detail: EventDetail::CapLifecycle { op, obj, source, dest },
        }
    }

    /// 构造 IPC 事件（不接收 payload 参数——从 API 层面杜绝泄密）。
    pub fn ipc(
        actor: AgentId,
        dir: IpcDir,
        endpoint: ObjRef,
        label: u32,
        timestamp: u64,
    ) -> AuditEvent {
        AuditEvent {
            kind: EventKind::Ipc,
            timestamp,
            actor,
            detail: EventDetail::Ipc { dir, endpoint, label },
        }
    }

    /// 构造进程生命周期事件。
    pub fn process(
        actor: AgentId,
        op: ProcOp,
        agent: AgentId,
        parent_pid: u32,
        code: i32,
        timestamp: u64,
    ) -> AuditEvent {
        AuditEvent {
            kind: EventKind::Process,
            timestamp,
            actor,
            detail: EventDetail::Process { op, agent, parent_pid, code },
        }
    }

    /// 构造审计服务系统事件。
    pub fn system(event: SystemEvent, param: u32, timestamp: u64) -> AuditEvent {
        AuditEvent {
            kind: EventKind::System,
            timestamp,
            actor: AgentId::UNSTAMPED,
            detail: EventDetail::System { event, param },
        }
    }
}
