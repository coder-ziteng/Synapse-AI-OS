//! FR9 审计事件流内核接线（P4-T11）。
//!
//! ## 职责
//!
//! 把 `synapse-audit` 纯逻辑 crate 接入内核：持有全局 append-only 环形队列，
//! 在内核事件点（cap 校验 / IPC send/recv/reply / 进程 spawn/exit/fault/reap）
//! 构造事件并盖章（actor agent_id + monotonic_ns 时间戳）入队。
//!
//! ## 锁序（防死锁契约）
//!
//! `AUDIT` 是**叶子锁**：持有 AUDIT 期间禁止获取任何 kstate 锁
//! （OBJECTS/PROCS/CAP_TABLES/ENDPOINTS/NOTIFICATIONS）或 SCHED/AUX。
//! 事件构造函数先在独立临界区查 `PROCS`（pid → agent_id 盖章），放锁后
//! 再进 AUDIT——两锁**顺序获取，永不嵌套**。
//!
//! ## 溢出策略（Doc 03 §6.3，decision_log: logs/t11-decisions.txt）
//!
//! 队满**覆盖最旧**（drop-oldest）+ `overflow` 计数——审计永不阻塞内核
//! 热路径（NFR2 优先）；丢失可见不静默：push 侧打 warn，查询侧暴露
//! [`overflow()`。
//!
//! ## 查询接口（内核侧 QUERY，Doc 01 AuditLog 行）
//!
//! MVP 无独立审计服务进程（S5, P5+）——查询能力先以内核 API 暴露：
//! [`len`] / [`overflow`] / [`drain`]（批量出队，最旧优先）。P5 审计服务
//! 化后 drain 改走 IPC 批量提交，接口不变。
//!
//! ## 泄密防线
//!
//! 事件构造走 `AuditEvent` 的封闭 API（无 payload 字段）——本模块不新增
//! 任何携带用户态业务数据的通道。

use core::sync::atomic::{AtomicU64, Ordering};

use log::{info, warn};
use synapse_audit::{
    AuditEvent, AuditQueue, IpcDir, ProcOp, SystemEvent, DEFAULT_QUEUE_CAPACITY,
};
use synapse_cap::{ObjKind, ObjRef, Rights};
use synapse_ipc::AgentId;
use synapse_proc::process::Pid;

use crate::sync::SpinLock;

/// 队列容量（256 条 × ~40B ≈ 10KB 常驻；构造期栈上临时对象同量级，
/// 满足 kstate "临时对象 ≤ 20KB" 的栈安全约束）。
pub const AUDIT_CAPACITY: usize = DEFAULT_QUEUE_CAPACITY;

/// 全局审计队列（Option 模式与 kstate 一致：boot 期 audit_init 就地构造）。
static AUDIT: SpinLock<Option<AuditQueue<AUDIT_CAPACITY>>> = SpinLock::new(None);

/// 累计入队条数（drain 不回退——启动链验证用总量）。
static AUDIT_PUSHED: AtomicU64 = AtomicU64::new(0);

/// cap 校验失败且**目标对象未能解析**时的哨兵 ObjRef（get/rights 失败路径
/// 拿不到真实对象引用；index=u32::MAX 与合法对象表索引空间不重叠）。
pub const UNKNOWN_OBJ: ObjRef = ObjRef {
    index: u32::MAX,
    generation: 0,
};

// ---------------------------------------------------------------------------
// 生命周期
// ---------------------------------------------------------------------------

/// boot 期初始化：构造队列 + 推 `System(Startup)` 首事件。
///
/// 调用点：main.rs `clock::calibrate()` 之后（时间戳有意义）、
/// `run_integration_smoke` 之前（后续所有内核事件点均被覆盖）。
/// 重复调用 panic。
pub fn audit_init() {
    {
        let mut g = AUDIT.lock();
        if g.is_some() {
            panic!("[audit] audit_init called twice");
        }
        *g = Some(AuditQueue::new());
    }
    push(AuditEvent::system(SystemEvent::Startup, AUDIT_CAPACITY as u32, now()));
    info!(
        "[audit] initialized: append-only ring queue, capacity={} (FR9)",
        AUDIT_CAPACITY
    );
}

// ---------------------------------------------------------------------------
// 事件点入口（内核各处调用；全部 O(1)、永不阻塞、永不失败）
// ---------------------------------------------------------------------------

/// cap 校验事件（`resolve_endpoint` 成功/失败路径）。
///
/// `target` 为被校验对象；解析前失败（slot 空 / NULL trap）用 [`UNKNOWN_OBJ`]。
pub fn cap_verify(pid: u32, target: ObjRef, rights: Rights, ok: bool) {
    let actor = agent_of(pid);
    push(AuditEvent::cap_verify(actor, target, rights, ok, now()));
}

/// cap 生命周期事件（mint/delegate/revoke 接线点见 kstate；P4-T11 先接
/// mint，delegate/revoke 随 P4-T13 capability syscall 接线补齐）。
pub fn cap_lifecycle(actor_pid: u32, op: synapse_audit::CapOp, obj: ObjKind, dest: AgentId) {
    let actor = agent_of(actor_pid);
    push(AuditEvent::cap_lifecycle(actor, op, obj, actor, dest, now()));
}

/// IPC 关键路径事件（send/recv/reply；label 为消息标签，**不含 payload**）。
pub fn ipc(pid: u32, dir: IpcDir, endpoint: ObjRef, label: u32) {
    let actor = agent_of(pid);
    push(AuditEvent::ipc(actor, dir, endpoint, label, now()));
}

/// 进程生命周期事件（通用入口；actor/agent 由调用方显式给出——terminate
/// 路径 PCB 可能已 Zombie/reap，不能再依赖 `agent_of` 反查）。
pub fn process_ev(actor: AgentId, op: ProcOp, agent: AgentId, parent_pid: u32, code: i32) {
    push(AuditEvent::process(actor, op, agent, parent_pid, code, now()));
}

/// spawn 便捷入口（调用点：spawn.rs do_spawn / ipc_pong_smoke do_pong_spawn）。
pub fn process_spawn(parent_pid: u32, child_pid: u32, child_agent: AgentId) {
    let actor = agent_of(parent_pid);
    process_ev(actor, ProcOp::Spawn, child_agent, parent_pid, child_pid as i32);
}

// ---------------------------------------------------------------------------
// 查询接口（内核侧 QUERY；P5 审计服务化后改走 IPC 批量提交）
// ---------------------------------------------------------------------------

/// 当前队列存量。
pub fn len() -> usize {
    AUDIT.lock().as_ref().map(|q| q.len()).unwrap_or(0)
}

/// 累计入队条数（drain 不回退）。
pub fn pushed_total() -> u64 {
    AUDIT_PUSHED.load(Ordering::SeqCst)
}

/// 累计覆盖（丢弃）条数——溢出可见性（丢失不静默）。
pub fn overflow() -> u64 {
    AUDIT.lock().as_ref().map(|q| q.overflow()).unwrap_or(0)
}

/// 批量出队（最旧优先），返回实际条数。
pub fn drain(out: &mut [AuditEvent]) -> usize {
    AUDIT
        .lock()
        .as_mut()
        .map(|q| q.drain(out))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

/// 时间戳源：TSC 折算单调纳秒（未校准期返回 0，仍单调不减）。
fn now() -> u64 {
    crate::clock::monotonic_ns()
}

/// pid → agent_id 盖章（★ 内核盖章，用户态不可伪造）。
///
/// PCB 不存在（已 reap / kthread smoke 伪 pid）→ `AgentId::UNSTAMPED`。
/// pub：proc_life::sys_reap 等调用方需自行盖章 actor（独立临界区，锁序安全）。
pub fn agent_of_pub(pid: u32) -> AgentId {
    agent_of(pid)
}

fn agent_of(pid: u32) -> AgentId {
    crate::kstate::with_procs(|t| {
        t.get(Pid(pid))
            .map(|pcb| pcb.agent)
            .unwrap_or(AgentId::UNSTAMPED)
    })
}

/// 入队（叶子锁；队满覆盖最旧 + warn 一次/条）。audit_init 前调用 = 丢弃
/// （boot 早期 marker 阶段无事件点，实际不发生）。
fn push(ev: AuditEvent) {
    let mut g = AUDIT.lock();
    let Some(q) = g.as_mut() else { return };
    if q.push(ev).is_some() {
        warn!(
            "[audit] queue overflow: oldest event dropped (cumulative={})",
            q.overflow()
        );
    }
    AUDIT_PUSHED.fetch_add(1, Ordering::SeqCst);
}
